//! Streaming client: connects to an [`crate::server`], negotiates the format,
//! receives audio (UDP or TCP) and feeds a [`sink::PcmSink`].
//!
//! Includes automatic reconnection with backoff, sequence-based loss
//! accounting and RTT probing — improvements over the original client.

pub mod resample;
pub mod sink;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc as tsc, watch};

use crate::protocol::{self, AudioFormat, Frame, FrameType, Hello};
use sink::{NullSinkFactory, PcmSink, SinkFactory};

/// Sender half of the live connection's writer channel.
type SharedWriter = Arc<Mutex<Option<tsc::Sender<Arc<Vec<u8>>>>>>;
/// Current playback sink shared between the data path and stats polling.
type SharedSink = Arc<Mutex<Box<dyn PcmSink>>>;
pub struct ClientConfig {
    /// Server host (IP or DNS name).
    pub host: String,
    pub port: u16,
    /// Request audio over the TCP channel (for UDP-blocked networks).
    pub tcp_data: bool,
    /// Reconnect forever until stopped.
    pub retry: bool,
    /// Sink factory (playback target). Defaults to [`NullSinkFactory`].
    pub sink_factory: Option<Box<dyn SinkFactory>>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: protocol::DEFAULT_PORT,
            tcp_data: false,
            retry: true,
            sink_factory: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientStats {
    pub connected: bool,
    pub server_name: String,
    pub format: Option<AudioFormat>,
    pub buffered_ms: u32,
    pub bytes_received: u64,
    pub lost_packets: u64,
    pub rtt_ms: Option<u64>,
    pub reconnects: u32,
    pub last_error: Option<String>,
    pub connected_since_ms: u64,
}

pub struct ClientHandle {
    stop: Arc<AtomicBool>,
    stats_rx: watch::Receiver<Arc<ClientStats>>,
    /// Sender of the CURRENT connection's writer channel (set/cleared by run_once).
    writer_slot: SharedWriter,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    started: bool,
}

impl ClientHandle {
    /// Latest client statistics.
    pub fn stats(&self) -> Arc<ClientStats> {
        self.stats_rx.borrow().clone()
    }

    pub fn subscribe_stats(&self) -> watch::Receiver<Arc<ClientStats>> {
        self.stats_rx.clone()
    }

    /// Disconnect (sends a graceful `Stop` frame best-effort).
    pub async fn stop(&mut self) {
        if !self.started {
            return;
        }
        self.started = false;
        // Graceful stop through the live connection's writer, if any.
        let writer = self.writer_slot.lock().unwrap().take();
        if let Some(w) = writer {
            let _ = w
                .send_timeout(
                    Arc::new(Frame::new(FrameType::Stop, 0, Vec::new()).encode()),
                    Duration::from_millis(200),
                )
                .await;
        }
        self.stop.store(true, Ordering::Relaxed);
        for t in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_millis(300), t).await;
        }
    }
}

#[derive(Default)]
struct SeqTracker {
    last: Option<u32>,
    lost: u64,
}

impl SeqTracker {
    fn track(&mut self, seq: u32) {
        if let Some(last) = self.last {
            let diff = seq.wrapping_sub(last);
            if diff > 1 && diff < 8192 {
                self.lost += (diff - 1) as u64;
            }
        }
        self.last = Some(seq);
    }
}

/// Run a client. Returns a handle; audio flows until [`ClientHandle::stop`].
pub async fn run_client(mut cfg: ClientConfig) -> Result<ClientHandle> {
    let (stats_tx, stats_rx) = watch::channel(Arc::new(ClientStats::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer_slot: SharedWriter = Arc::new(Mutex::new(None));

    let mut tasks = Vec::new();
    let factory: Arc<dyn SinkFactory> = cfg
        .sink_factory
        .take()
        .map(|b| Arc::from(b) as Arc<dyn SinkFactory>)
        .unwrap_or_else(|| Arc::new(NullSinkFactory));
    {
        let stop = Arc::clone(&stop);
        let stats_tx = stats_tx.clone();
        let writer_slot = Arc::clone(&writer_slot);
        let factory = Arc::clone(&factory);
        tasks.push(tokio::spawn(async move {
            session_loop(cfg, stop, stats_tx, writer_slot, factory).await;
        }));
    }

    Ok(ClientHandle {
        stop,
        stats_rx,
        writer_slot,
        tasks,
        started: true,
    })
}

async fn session_loop(
    cfg: ClientConfig,
    stop: Arc<AtomicBool>,
    stats_tx: watch::Sender<Arc<ClientStats>>,
    writer_slot: SharedWriter,
    factory: Arc<dyn SinkFactory>,
) {
    let bytes_received = Arc::new(AtomicU64::new(0));
    let mut reconnects = 0u32;
    let mut backoff = Duration::from_secs(1);
    let mut current_sink: Option<(AudioFormat, SharedSink)> = None;
    let mut stats = ClientStats::default();

    while !stop.load(Ordering::Relaxed) {
        match run_once(
            &cfg,
            &stop,
            &writer_slot,
            factory.as_ref(),
            &mut current_sink,
            &bytes_received,
            &mut stats,
            &stats_tx,
        )
        .await
        {
            Ok(()) => break, // graceful end
            Err(e) => {
                stats.connected = false;
                stats.last_error = Some(format!("{e:#}"));
                let _ = stats_tx.send(Arc::new(stats.clone()));
                tracing::warn!("client disconnected: {e:#}");
                // Rebuild the sink next attempt (playback state is dead).
                if let Some((_, old)) = current_sink.take() {
                    old.lock().unwrap().shutdown();
                }
                if !cfg.retry || stop.load(Ordering::Relaxed) {
                    break;
                }
                reconnects += 1;
                stats.reconnects = reconnects;
                tokio::select! {
                    _ = stop_changed(&stop) => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
    if let Some((_, old)) = current_sink.take() {
        old.lock().unwrap().shutdown();
    }
    stats.connected = false;
    let _ = stats_tx.send(Arc::new(stats));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredServer {
    /// Instance name advertised by the server.
    pub name: String,
    /// IP address of the server.
    pub host: String,
    pub port: u16,
}

/// Browse the LAN for running servers via mDNS (`_audiosharers._tcp.local.`).
/// Blocking — call from `spawn_blocking` in async contexts.
pub fn discover_servers(timeout: Duration) -> Result<Vec<DiscoveredServer>> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let daemon = ServiceDaemon::new().context("mDNS daemon")?;
    let receiver = daemon
        .browse("_audiosharers._tcp.local.")
        .context("mDNS browse")?;
    let deadline = Instant::now() + timeout;
    let mut out: Vec<DiscoveredServer> = Vec::new();
    while Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remain.min(Duration::from_millis(250))) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let name = info
                    .get_fullname()
                    .split('.')
                    .next()
                    .unwrap_or("server")
                    .to_string();
                let port = info.get_port();
                if let Some(ip) = info.get_addresses().iter().next() {
                    let host = ip.to_string();
                    if !out.iter().any(|d| d.host == host && d.port == port) {
                        out.push(DiscoveredServer { name, host, port });
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break, // recv timeout
        }
    }
    let _ = daemon.stop_browse("_audiosharers._tcp.local.");
    Ok(out)
}

async fn stop_changed(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    cfg: &ClientConfig,
    stop: &AtomicBool,
    writer_slot: &SharedWriter,
    factory: &dyn SinkFactory,
    current_sink: &mut Option<(AudioFormat, SharedSink)>,
    bytes_received: &Arc<AtomicU64>,
    stats: &mut ClientStats,
    stats_tx: &watch::Sender<Arc<ClientStats>>,
) -> Result<()> {
    let addr = (cfg.host.as_str(), cfg.port);
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))?
        .with_context(|| format!("connect to {}:{} failed", cfg.host, cfg.port))?;
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();

    let (wtx, mut wrx) = tsc::channel::<Arc<Vec<u8>>>(512);
    // Publish the live writer so ClientHandle::stop() can send a Stop frame.
    *writer_slot.lock().unwrap() = Some(wtx.clone());
    // Local writer task feeding the socket.
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = wrx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    async fn read_frame(
        rd: &mut tokio::net::tcp::OwnedReadHalf,
        timeout: Duration,
    ) -> Result<Frame> {
        let mut head = [0u8; Frame::HEADER_LEN];
        tokio::time::timeout(timeout, rd.read_exact(&mut head))
            .await
            .map_err(|_| anyhow::anyhow!("read timeout"))?
            .context("connection closed")?;
        let len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if len > protocol::MAX_PAYLOAD_LEN {
            bail!("oversized frame");
        }
        let mut payload = vec![0u8; len];
        tokio::time::timeout(timeout, rd.read_exact(&mut payload))
            .await
            .map_err(|_| anyhow::anyhow!("read timeout"))?
            .context("connection closed")?;
        let mut buf = head.to_vec();
        buf.extend_from_slice(&payload);
        let (f, _) = Frame::decode(&buf)?;
        Ok(f)
    }

    // 1) Hello
    let hello_frame = read_frame(&mut rd, Duration::from_secs(5)).await?;
    let hello = if hello_frame.ftype == FrameType::Hello {
        Hello::decode(&hello_frame.payload)?
    } else {
        bail!("expected Hello, got {:?}", hello_frame.ftype);
    };
    tracing::info!(
        "server '{}': {} ({} ms blocks)",
        hello.name,
        hello.format,
        hello.block_ms
    );

    // 2) Sink for this format (rebuilt whenever the format changes)
    let need_new_sink = match current_sink {
        Some((fmt, _)) => *fmt != hello.format,
        None => true,
    };
    if need_new_sink {
        if let Some((_, old)) = current_sink.take() {
            old.lock().unwrap().shutdown();
        }
        let built = factory
            .build(&hello.format)
            .context("failed to open playback sink")?;
        *current_sink = Some((hello.format, Arc::new(Mutex::new(built))));
    }
    let sink = Arc::clone(&current_sink.as_ref().unwrap().1);

    // 3) Start
    let start = protocol::start_frame(cfg.tcp_data);
    wtx.send(Arc::new(start.encode()))
        .await
        .context("writer closed")?;
    let started = read_frame(&mut rd, Duration::from_secs(5)).await?;
    if started.ftype != FrameType::Started {
        bail!("expected Started, got {:?}", started.ftype);
    }
    let session = u32::from_le_bytes(started.payload[..4].try_into().context("short session")?);
    tracing::info!(
        "playing — session {session}, {} mode",
        if cfg.tcp_data { "TCP" } else { "UDP" }
    );

    stats.connected = true;
    stats.server_name = hello.name.clone();
    stats.format = Some(hello.format);
    stats.last_error = None;
    stats.connected_since_ms = crate::server::now_ms();
    let _ = stats_tx.send(Arc::new(stats.clone()));

    let idle_timeout = Duration::from_secs(15);
    let tracker = Arc::new(Mutex::new(SeqTracker::default()));
    let rtt: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    // 4) UDP data path (only when TCP audio is not requested)
    let mut udp_task: Option<tokio::task::JoinHandle<()>> = None;
    if !cfg.tcp_data {
        let sink = Arc::clone(&sink);
        let received = Arc::clone(bytes_received);
        let host = cfg.host.clone();
        let port = cfg.port;
        udp_task = Some(tokio::spawn(async move {
            if let Err(e) = udp_loop(host, port, session, sink, received).await {
                tracing::debug!("udp loop ended: {e:#}");
            }
        }));
    }

    // 5) RTT probes (server echoes STATS frames back)
    let probe_task = {
        let wtx = wtx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let f = protocol::stats_frame(crate::server::now_ms());
                if wtx.send(Arc::new(f.encode())).await.is_err() {
                    break;
                }
            }
        })
    };

    // 6) stats publisher
    let stats_task = {
        let sink = Arc::clone(&sink);
        let received = Arc::clone(bytes_received);
        let tracker = Arc::clone(&tracker);
        let rtt = Arc::clone(&rtt);
        let mut base = stats.clone();
        let stats_tx = stats_tx.clone();
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        (
            done_tx,
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(500));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = done_rx.changed() => break,
                        _ = tick.tick() => {
                            base.connected = true;
                            base.buffered_ms = sink.lock().unwrap().buffered_ms();
                            base.bytes_received = received.load(Ordering::Relaxed);
                            base.lost_packets = tracker.lock().unwrap().lost;
                            base.rtt_ms = *rtt.lock().unwrap();
                            let _ = stats_tx.send(Arc::new(base.clone()));
                        }
                    }
                }
            }),
        )
    };

    let result: Result<()> = loop {
        tokio::select! {
            _ = stop_changed(stop) => {
                let _ = wtx.send(Arc::new(Frame::new(FrameType::Stop, 0, Vec::new()).encode())).await;
                break Ok(());
            }
            r = read_frame(&mut rd, idle_timeout) => match r {
                Ok(frame) => match frame.ftype {
                    FrameType::Audio => {
                        let (seq, pcm) = protocol::parse_tcp_audio(&frame.payload)?;
                        tracker.lock().unwrap().track(seq);
                        bytes_received.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                        sink.lock().unwrap().push(seq, pcm);
                    }
                    FrameType::Ping => {
                        if frame.payload.len() == 8 {
                            let ts = u64::from_le_bytes(frame.payload[..8].try_into()?);
                            let _ = wtx.send(Arc::new(protocol::pong_frame(ts).encode())).await;
                        }
                    }
                    FrameType::Stats => {
                        if frame.payload.len() == 8 {
                            let ts = u64::from_le_bytes(frame.payload[..8].try_into()?);
                            let r = crate::server::now_ms().saturating_sub(ts);
                            if r < 60_000 {
                                *rtt.lock().unwrap() = Some(r);
                            }
                        }
                    }
                    FrameType::Stop => break Ok(()),
                    _ => {}
                },
                Err(e) => break Err(e),
            },
        }
    };

    if let Some(t) = udp_task {
        t.abort();
    }
    probe_task.abort();
    let (done_tx, stats_task) = stats_task;
    let _ = done_tx.send(true);
    let _ = stats_task.await;
    sink.lock().unwrap().shutdown();
    stats.connected = false;
    stats.lost_packets = tracker.lock().unwrap().lost;
    stats.rtt_ms = *rtt.lock().unwrap();
    // Release the slot BEFORE awaiting the writer so it can observe channel
    // closure — otherwise cleanup deadlocks and stats never publish the error.
    *writer_slot.lock().unwrap() = None;
    drop(wtx);
    let _ = writer_task.await;
    result
}

async fn udp_loop(
    host: String,
    port: u16,
    session: u32,
    sink: SharedSink,
    received: Arc<AtomicU64>,
) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect((host.as_str(), port)).await?;
    tracing::debug!("client udp connected to {host}:{port} (session {session})");

    // Register + keep-alive: send the session token every second.
    let mut keepalive = tokio::time::interval(Duration::from_secs(1));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut buf = vec![0u8; 2048];
    let mut tracker = SeqTracker::default();
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if sock.send(&session.to_le_bytes()).await.is_err() {
                    bail!("udp send failed");
                }
            }
            r = sock.recv(&mut buf) => match r {
                Ok(n) if n >= 8 => {
                    let (s, seq, pcm) = protocol::parse_udp_audio(&buf[..n])?;
                    if s != session {
                        continue;
                    }
                    tracker.track(seq);
                    received.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                    sink.lock().unwrap().push(seq, pcm);
                }
                Ok(_) => continue,
                Err(e) => bail!("udp recv: {e}"),
            },
        }
    }
}
