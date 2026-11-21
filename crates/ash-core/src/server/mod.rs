//! LAN streaming server: captures audio and broadcasts it to any number of
//! players over UDP (data) + TCP (control, optional data).
//!
//! Task layout (all inside the caller's tokio runtime):
//! - capturer thread (std) → bridge thread → broadcast channel
//! - `pump`        — chunks → MTU segments → UDP datagrams + TCP audio frames
//! - `tcp_accept`  — accepts control connections, one session per client
//! - `session`     — handshake, heartbeat replies, per-client writer task
//! - `udp_server`  — session-token registration / endpoint refresh
//! - `heartbeat`   — pings peers, expires silent ones
//! - `stats`       — publishes a [`ServerStats`] snapshot every second
//! - optional mDNS advertisement (`_audiosharers._tcp`)

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc as tsc, watch};

use crate::capture::{open_capturer, Capturer, SourceSelection};
use crate::net::default_lan_address;
use crate::protocol::{self, AudioFormat, Encoding, Frame, FrameType, Hello};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind (use `0.0.0.0` for all interfaces).
    pub host: IpAddr,
    /// TCP + UDP port (both are used).
    pub port: u16,
    /// Audio source selection.
    pub source: SourceSelection,
    /// Wire encoding (converted from the native float capture).
    pub encoding: Encoding,
    /// Server-side chunk size in milliseconds (scheduling granularity).
    pub block_ms: u32,
    /// Friendly name advertised in `Hello` and mDNS.
    pub name: String,
    /// Announce via mDNS `_audiosharers._tcp.local.`.
    pub mdns: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".parse().unwrap(),
            port: protocol::DEFAULT_PORT,
            source: SourceSelection::Loopback { device: None },
            encoding: Encoding::S16Le,
            block_ms: 20,
            name: "AudioShare-RS".into(),
            mdns: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientStat {
    pub session: u32,
    pub addr: String,
    pub tcp_data: bool,
    pub since_ms: u64,
    pub last_seen_ms: u64,
    pub rtt_ms: Option<u64>,
    pub sent_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ServerStats {
    pub running: bool,
    pub name: String,
    pub bind_addr: String,
    /// The TCP/UDP port the server is listening on.
    pub port: u16,
    pub format: AudioFormat,
    pub block_ms: u32,
    pub started_ms: u64,
    pub clients: Vec<ClientStat>,
    pub total_bytes: u64,
    pub udp_drops: u64,
    pub error: Option<String>,
}

struct Peer {
    addr: SocketAddr,
    udp: Option<SocketAddr>,
    audio_tx: Option<tsc::Sender<Arc<Vec<u8>>>>,
    /// Signalling channel used to terminate the session on timeout.
    kill: Option<tsc::Sender<()>>,
    tcp_data: bool,
    since_ms: u64,
    last_seen_ms: u64,
    rtt_ms: Option<u64>,
    sent_bytes: u64,
}

struct ServerMeta {
    name: String,
    bind_addr: String,
    port: u16,
    format: AudioFormat,
    block_ms: u32,
    started_ms: u64,
}

struct Shared {
    meta: ServerMeta,
    stats_tx: watch::Sender<Arc<ServerStats>>,
    peers: Mutex<HashMap<u32, Peer>>,
    total_bytes: AtomicU64,
    udp_drops: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn build_stats(&self) -> ServerStats {
        ServerStats {
            running: true,
            name: self.meta.name.clone(),
            bind_addr: self.meta.bind_addr.clone(),
            port: self.meta.port,
            format: self.meta.format,
            block_ms: self.meta.block_ms,
            started_ms: self.meta.started_ms,
            clients: {
                let peers = self.peers.lock().unwrap();
                peers
                    .iter()
                    .map(|(s, p)| ClientStat {
                        session: *s,
                        addr: p.addr.to_string(),
                        tcp_data: p.tcp_data,
                        since_ms: p.since_ms,
                        last_seen_ms: p.last_seen_ms,
                        rtt_ms: p.rtt_ms,
                        sent_bytes: p.sent_bytes,
                    })
                    .collect()
            },
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            udp_drops: self.udp_drops.load(Ordering::Relaxed),
            error: self.error.lock().unwrap().clone(),
        }
    }

    fn publish(&self) {
        let _ = self.stats_tx.send(Arc::new(self.build_stats()));
    }
}

/// Handle to a running server.
pub struct ServerHandle {
    shutdown: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    shutdown_tx: watch::Sender<bool>,
    stats_rx: watch::Receiver<Arc<ServerStats>>,
    capturer: Option<Box<dyn Capturer>>,
    mdns: Option<(mdns_sd::ServiceDaemon, String)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    bridge: Option<std::thread::JoinHandle<()>>,
    started: bool,
}

impl ServerHandle {
    /// Start capturing and accepting clients. Must be called from inside a
    /// tokio runtime.
    pub async fn start(cfg: ServerConfig) -> Result<Self> {
        let bind = SocketAddr::new(cfg.host, cfg.port);

        // --- capture source -------------------------------------------------
        let mut capturer =
            open_capturer(&cfg.source, cfg.encoding).context("failed to open capture source")?;
        let format = capturer.format();
        let bpf = format.bytes_per_frame();
        if bpf == 0 {
            bail!("invalid audio format");
        }
        let block_bytes = (format.bytes_per_second() / 1000 * cfg.block_ms as usize) / bpf * bpf;
        let block_bytes = block_bytes.max(bpf);

        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
        capturer.start(chunk_tx.clone())?;
        drop(chunk_tx); // only the capturer holds a sender now

        // capturer thread -> block-aligned chunks -> broadcast
        let (bcast_tx, bcast_rx) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let bridge = std::thread::Builder::new()
            .name("ash-bridge".into())
            .spawn(move || {
                let mut buf: Vec<u8> = Vec::with_capacity(block_bytes * 2);
                for chunk in chunk_rx {
                    buf.extend(chunk);
                    while buf.len() >= block_bytes {
                        let out: Vec<u8> = buf.drain(..block_bytes).collect();
                        if bcast_tx.send(Arc::new(out)).is_err() {
                            return; // pump gone: shutting down
                        }
                    }
                }
            })?;

        // --- TCP listener ---------------------------------------------------
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("TCP bind {bind} failed"))?;
        // Single UDP socket for both registration and audio data — clients
        // connect to this exact address, so data must originate from it.
        let udp_sock = Arc::new(
            UdpSocket::bind(bind)
                .await
                .with_context(|| format!("UDP bind {bind} failed"))?,
        );
        // TCP and UDP must share the same port number (protocol requirement);
        // this is guaranteed whenever a concrete port is configured.
        let actual = listener.local_addr()?;
        // --- shared state ---------------------------------------------------
        let (stats_tx, stats_rx) = watch::channel(Arc::new(ServerStats {
            running: true,
            name: cfg.name.clone(),
            bind_addr: actual.to_string(),
            port: actual.port(),
            format,
            block_ms: cfg.block_ms,
            started_ms: now_ms(),
            clients: Vec::new(),
            total_bytes: 0,
            udp_drops: 0,
            error: None,
        }));
        let shared = Arc::new(Shared {
            meta: ServerMeta {
                name: cfg.name.clone(),
                bind_addr: actual.to_string(),
                port: actual.port(),
                format,
                block_ms: cfg.block_ms,
                started_ms: now_ms(),
            },
            peers: Mutex::new(HashMap::new()),
            total_bytes: AtomicU64::new(0),
            udp_drops: AtomicU64::new(0),
            error: Mutex::new(None),
            stats_tx: stats_tx.clone(),
        });
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shutdown = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());

        let mut tasks = Vec::new();

        // accept loop
        {
            let shared = Arc::clone(&shared);
            let mut shutdown_rx = shutdown_rx.clone();
            let block_ms = cfg.block_ms as u16;
            let name = cfg.name.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = wait_shutdown(&mut shutdown_rx) => break,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, addr)) => {
                                let _ = stream.set_nodelay(true);
                                let shared = Arc::clone(&shared);
                                let shutdown_rx = shutdown_rx.clone();
                                let name = name.clone();
                                let (wtx, wrx) = tsc::channel::<Arc<Vec<u8>>>(1024);
                                let (kill_tx, kill_rx) = tsc::channel::<()>(1);
                                tokio::spawn(async move {
                                    run_session(
                                        stream, addr, shared, shutdown_rx,
                                        format, block_ms, name, wtx, wrx, kill_tx, kill_rx,
                                    ).await;
                                });
                            }
                            Err(e) => {
                                tracing::warn!("accept failed: {e}");
                                tokio::time::sleep(Duration::from_millis(200)).await;
                            }
                        }
                    }
                }
            }));
        }

        // udp server (session-token registration / endpoint refresh)
        {
            let shared = Arc::clone(&shared);
            let mut shutdown_rx = shutdown_rx.clone();
            let udp_sock = Arc::clone(&udp_sock);
            tasks.push(tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    tokio::select! {
                        _ = wait_shutdown(&mut shutdown_rx) => break,
                        r = udp_sock.recv_from(&mut buf) => match r {
                            Ok((n, from)) => {
                                if n < 4 { continue; }
                                let session = u32::from_le_bytes([
                                    buf[0], buf[1], buf[2], buf[3],
                                ]);
                                let mut peers = shared.peers.lock().unwrap();
                                if let Some(peer) = peers.get_mut(&session) {
                                    peer.udp = Some(from);
                                    peer.last_seen_ms = now_ms();
                                    } else {
                                    tracing::debug!("unknown udp session {session} from {from}");
                                }
                            }
                            Err(e) => {
                                tracing::debug!("udp recv error: {e}");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }));
        }

        // pump: broadcast chunks -> udp datagrams + tcp audio frames
        {
            let shared = Arc::clone(&shared);
            let mut shutdown_rx = shutdown_rx.clone();
            let mut bcast_rx = bcast_rx;
            let udp_sock = Arc::clone(&udp_sock);
            let step = (protocol::MTU_DATA_MAX / bpf) * bpf;
            let block_dur = Duration::from_millis(cfg.block_ms as u64);
            let silence = Arc::new(vec![0u8; block_bytes]);
            tasks.push(tokio::spawn(async move {
                let mut seq: u32 = 0;
                loop {
                    let chunk = tokio::select! {
                        _ = wait_shutdown(&mut shutdown_rx) => break,
                        r = tokio::time::timeout(block_dur, bcast_rx.recv()) => match r {
                            Ok(Ok(bytes)) => bytes,
                            Ok(Err(_)) => break, // capture bridge gone
                            Err(_elapsed) => Arc::clone(&silence), // silence fill
                        }
                    };
                    seq = seq.wrapping_add(1);
                    // Split into MTU-sized, frame-aligned segments.
                    let bytes: &[u8] = &chunk;
                    let mut segments: Vec<(u32, &[u8])> = Vec::new();
                    let mut off = 0usize;
                    let mut sub = 0u32;
                    while off < bytes.len() {
                        let mut end = off + (((off + step).min(bytes.len()) - off) / bpf) * bpf;
                        if end == off {
                            end = (off + bpf).min(bytes.len());
                        }
                        let s = seq.wrapping_add(sub);
                        segments.push((s, &bytes[off..end]));
                        sub = sub.wrapping_add(1);
                        off = end;
                        if sub > 4096 {
                            break;
                        }
                    }
                    let datagrams: Vec<(SocketAddr, Vec<u8>)> = {
                        let peers = shared.peers.lock().unwrap();
                        peers
                            .iter()
                            .filter_map(|(session, p)| p.udp.map(|ep| (*session, ep)))
                            .flat_map(|(session, ep)| {
                                segments.iter().map(move |(s, pcm)| {
                                    (ep, protocol::udp_audio_datagram(session, *s, pcm))
                                })
                            })
                            .collect()
                    };
                    for (ep, dg) in datagrams {
                        let _ = udp_sock.send_to(&dg, ep).await;
                    }
                    for (s, pcm) in &segments {
                        let payload = protocol::tcp_audio_payload(*s, pcm);
                        let frame = Arc::new(Frame::new(FrameType::Audio, 0, payload).encode());
                        let peers = shared.peers.lock().unwrap();
                        for p in peers.values().filter(|p| p.tcp_data) {
                            if let Some(tx) = &p.audio_tx {
                                if tx.try_send(Arc::clone(&frame)).is_err() {
                                    shared.udp_drops.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    shared
                        .total_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    {
                        let mut peers = shared.peers.lock().unwrap();
                        for p in peers.values_mut() {
                            p.sent_bytes += bytes.len() as u64;
                        }
                    }
                }
            }));
        }

        // heartbeat + expiry
        {
            let shared = Arc::clone(&shared);
            let mut shutdown_rx = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = wait_shutdown(&mut shutdown_rx) => break,
                        _ = tick.tick() => {
                            let mut expired = Vec::new();
                            {
                                let peers = shared.peers.lock().unwrap();
                                let now = now_ms();
                                for (session, p) in peers.iter() {
                                    if now.saturating_sub(p.last_seen_ms)
                                        > protocol::HEARTBEAT_TIMEOUT.as_millis() as u64
                                    {
                                        expired.push(*session);
                                    }
                                }
                            }
                            if !expired.is_empty() {
                                let kills: Vec<tsc::Sender<()>> = {
                                    let mut peers = shared.peers.lock().unwrap();
                                    expired
                                        .iter()
                                        .filter_map(|s| {
                                            let p = peers.remove(s)?;
                                            tracing::info!("peer {} ({}) timed out", s, p.addr);
                                            p.kill
                                        })
                                        .collect()
                                };
                                for k in kills {
                                    let _ = k.send(()).await;
                                }
                            }
                            // ping everyone
                            let pings: Vec<(u32, tsc::Sender<Arc<Vec<u8>>>)> = {
                                let peers = shared.peers.lock().unwrap();
                                peers
                                    .iter()
                                    .filter_map(|(s, p)| p.audio_tx.clone().map(|tx| (*s, tx)))
                                    .collect()
                            };
                            let frame = Arc::new(protocol::ping_frame(now_ms()).encode());
                            for (_s, tx) in pings {
                                let _ = tx.try_send(Arc::clone(&frame));
                            }
                        }
                    }
                }
            }));
        }

        // stats publisher (ticker; sessions also publish on register/remove)
        {
            let shared = Arc::clone(&shared);
            let mut shutdown_rx = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = wait_shutdown(&mut shutdown_rx) => break,
                        _ = tick.tick() => shared.publish(),
                    }
                }
            }));
        }

        // --- mDNS advertisement ---------------------------------------------
        let mdns = if cfg.mdns {
            match register_mdns(&cfg) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("mDNS registration failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        tracing::info!(
            "server up: {} on {} — {} @ {} Hz, {} ch, {} ms blocks",
            cfg.name,
            bind,
            format,
            format.sample_rate,
            format.channels,
            cfg.block_ms
        );

        Ok(Self {
            shutdown,
            notify,
            shutdown_tx,
            stats_rx,
            capturer: Some(capturer),
            mdns,
            tasks,
            bridge: Some(bridge),
            started: true,
        })
    }

    /// Latest stats snapshot.
    pub fn stats(&self) -> Arc<ServerStats> {
        self.stats_rx.borrow().clone()
    }

    pub fn subscribe_stats(&self) -> watch::Receiver<Arc<ServerStats>> {
        self.stats_rx.clone()
    }

    /// Stop the server: release capture, drop clients, close sockets.
    pub async fn stop(&mut self) {
        if !self.started {
            return;
        }
        self.started = false;
        let _ = self.shutdown_tx.send(true);
        self.shutdown.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
        if let Some(mut cap) = self.capturer.take() {
            cap.stop();
        }
        if let Some((daemon, fullname)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
        }
        for t in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_millis(500), t).await;
        }
        if let Some(b) = self.bridge.take() {
            let _ = b.join();
        }
        tracing::info!("server stopped");
    }
}

async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    stream: TcpStream,
    addr: SocketAddr,
    shared: Arc<Shared>,
    mut shutdown_rx: watch::Receiver<bool>,
    format: AudioFormat,
    block_ms: u16,
    name: String,
    wtx: tsc::Sender<Arc<Vec<u8>>>,
    mut wrx: tsc::Receiver<Arc<Vec<u8>>>,
    kill_tx: tsc::Sender<()>,
    mut kill_rx: tsc::Receiver<()>,
) {
    let (rd, mut wr) = stream.into_split();

    // writer task: serialize all outgoing frames
    let writer = tokio::spawn(async move {
        while let Some(bytes) = wrx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // send hello
    let hello = Frame::new(
        FrameType::Hello,
        0,
        Hello {
            format,
            block_ms,
            name,
        }
        .encode(),
    )
    .encode();
    if wtx.send(Arc::new(hello)).await.is_err() {
        return;
    }

    let mut session: Option<u32> = None;
    let mut head = [0u8; Frame::HEADER_LEN];
    let mut rd = rd;

    'outer: loop {
        let read = tokio::select! {
            _ = wait_shutdown(&mut shutdown_rx) => break,
            _ = kill_rx.recv() => break,
            r = rd.read_exact(&mut head) => r,
        };
        if read.is_err() {
            break;
        }
        let mut framed = head.to_vec();
        let len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if len > protocol::MAX_PAYLOAD_LEN {
            break;
        }
        let mut payload = vec![0u8; len];
        if rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        framed.extend_from_slice(&payload);
        let (frame, _) = match Frame::decode(&framed) {
            Ok(v) => v,
            Err(_) => break,
        };
        match frame.ftype {
            FrameType::Start => {
                let tcp_data = frame.flags & 1 != 0;
                // pick a fresh unique session token
                let mut s = rand::random::<u32>();
                while s == 0 {
                    s = rand::random::<u32>();
                }
                {
                    let mut peers = shared.peers.lock().unwrap();
                    while peers.contains_key(&s) {
                        s = rand::random::<u32>();
                    }
                    peers.insert(
                        s,
                        Peer {
                            addr,
                            udp: None,
                            audio_tx: Some(wtx.clone()),
                            kill: Some(kill_tx.clone()),
                            tcp_data,
                            since_ms: now_ms(),
                            last_seen_ms: now_ms(),
                            rtt_ms: None,
                            sent_bytes: 0,
                        },
                    );
                    if let Some(old) = session {
                        peers.remove(&old);
                    }
                }
                session = Some(s);
                tracing::info!("peer registered: session {s} tcp://{addr} tcp-data={tcp_data}");
                shared.publish();
                let started = Arc::new(protocol::started_frame(s).encode());
                if wtx.send(started).await.is_err() {
                    break 'outer;
                }
            }
            FrameType::Pong => {
                if frame.payload.len() == 8 {
                    let ts = u64::from_le_bytes(frame.payload[..8].try_into().unwrap());
                    let rtt = now_ms().saturating_sub(ts);
                    if let Some(s) = session {
                        if let Some(p) = shared.peers.lock().unwrap().get_mut(&s) {
                            p.last_seen_ms = now_ms();
                            if rtt < 60_000 {
                                p.rtt_ms = Some(rtt);
                            }
                        }
                    }
                }
            }
            FrameType::Stats => {
                if let Some(s) = session {
                    if let Some(p) = shared.peers.lock().unwrap().get_mut(&s) {
                        p.last_seen_ms = now_ms();
                    }
                }
                // echo back so the client can measure RTT
                let echo = Arc::new(Frame::new(FrameType::Stats, 0, frame.payload).encode());
                if wtx.send(echo).await.is_err() {
                    break 'outer;
                }
            }
            FrameType::Stop => {
                tracing::info!("peer {addr} sent stop");
                break 'outer;
            }
            _ => {}
        }
    }

    if let Some(s) = session {
        shared.peers.lock().unwrap().remove(&s);
        tracing::info!("peer removed: session {s} tcp://{addr}");
        shared.publish();
    }
    drop(wtx);
    let _ = writer.await;
}

fn register_mdns(cfg: &ServerConfig) -> Result<(mdns_sd::ServiceDaemon, String)> {
    use mdns_sd::ServiceInfo;
    let daemon = mdns_sd::ServiceDaemon::new().context("mdns daemon")?;
    let ip = if cfg.host.is_unspecified() {
        default_lan_address().context("no LAN address for mDNS")?
    } else {
        cfg.host
    };
    let instance = format!("{} @ {}:{}", cfg.name, ip, cfg.port);
    let host = format!("ashr-{:08x}.local.", rand::random::<u32>());
    let service = ServiceInfo::new(
        "_audiosharers._tcp.local.",
        &instance,
        &host,
        ip.to_string(),
        cfg.port,
        None,
    )?;
    daemon.register(service)?;
    tracing::info!("mDNS advertised: {instance}");
    Ok((daemon, format!("{instance}._audiosharers._tcp.local.")))
}
