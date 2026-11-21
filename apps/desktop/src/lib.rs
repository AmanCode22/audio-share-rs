//! AudioShare Companion — Tauri 2 desktop app (GTK/WebKitGTK on Linux,
//! native WebViews elsewhere). Wraps `ash-core` to provide both server and
//! client modes with live stats over a `tick` event.

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use ash_core::capture::{list_input_devices, list_loopback_devices, SourceSelection};
use ash_core::client::sink::{list_output_devices, CpalSinkFactory};
use ash_core::client::{run_client, ClientConfig, ClientHandle, ClientStats};
use ash_core::net::lan_addresses;
use ash_core::protocol::Encoding;
use ash_core::server::{ServerConfig, ServerHandle, ServerStats};

#[derive(Default)]
struct Engine {
    server: Option<ServerHandle>,
    client: Option<ClientHandle>,
}

type EngineState<'a> = State<'a, Mutex<Engine>>;

// ---------- DTOs ----------

#[derive(Serialize, Deserialize)]
pub struct DeviceDto {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_default: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ServerOptionsDto {
    pub host: Option<String>,
    pub port: u16,
    /// "loopback" | "mic" | "sine"
    pub source: String,
    pub device: Option<String>,
    /// "s16" | "f32"
    pub encoding: String,
    pub block_ms: u32,
    pub name: String,
    pub mdns: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ClientOptionsDto {
    pub host: String,
    pub port: u16,
    pub tcp_data: bool,
    pub output_device: Option<String>,
    pub buffer_ms: u32,
    pub volume: f32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ClientStatDto {
    pub session: u32,
    pub addr: String,
    pub tcp_data: bool,
    pub rtt_ms: Option<u64>,
    pub sent_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerStatsDto {
    pub running: bool,
    pub name: String,
    pub bind_addr: String,
    pub port: u16,
    pub format: String,
    pub block_ms: u32,
    pub clients: Vec<ClientStatDto>,
    pub total_bytes: u64,
    pub udp_drops: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ClientStatsDto {
    pub connected: bool,
    pub server_name: String,
    pub format: String,
    pub buffered_ms: u32,
    pub bytes_received: u64,
    pub lost_packets: u64,
    pub rtt_ms: Option<u64>,
    pub reconnects: u32,
    pub last_error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DiscoveredDto {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Serialize, Clone)]
pub struct TickPayload {
    pub server: ServerStatsDto,
    pub client: ClientStatsDto,
}

impl From<&ServerStats> for ServerStatsDto {
    fn from(s: &ServerStats) -> Self {
        Self {
            running: s.running,
            name: s.name.clone(),
            bind_addr: s.bind_addr.clone(),
            port: s.port,
            format: s.format.to_string(),
            block_ms: s.block_ms,
            clients: s
                .clients
                .iter()
                .map(|c| ClientStatDto {
                    session: c.session,
                    addr: c.addr.clone(),
                    tcp_data: c.tcp_data,
                    rtt_ms: c.rtt_ms,
                    sent_bytes: c.sent_bytes,
                })
                .collect(),
            total_bytes: s.total_bytes,
            udp_drops: s.udp_drops,
        }
    }
}

impl From<&ClientStats> for ClientStatsDto {
    fn from(s: &ClientStats) -> Self {
        Self {
            connected: s.connected,
            server_name: s.server_name.clone(),
            format: s.format.map(|f| f.to_string()).unwrap_or_default(),
            buffered_ms: s.buffered_ms,
            bytes_received: s.bytes_received,
            lost_packets: s.lost_packets,
            rtt_ms: s.rtt_ms,
            reconnects: s.reconnects,
            last_error: s.last_error.clone(),
        }
    }
}

fn idle_server_dto() -> ServerStatsDto {
    ServerStatsDto {
        running: false,
        name: String::new(),
        bind_addr: String::new(),
        port: 0,
        format: String::new(),
        block_ms: 0,
        clients: vec![],
        total_bytes: 0,
        udp_drops: 0,
    }
}

fn idle_client_dto() -> ClientStatsDto {
    ClientStatsDto {
        connected: false,
        server_name: String::new(),
        format: String::new(),
        buffered_ms: 0,
        bytes_received: 0,
        lost_packets: 0,
        rtt_ms: None,
        reconnects: 0,
        last_error: None,
    }
}

// ---------- commands ----------

#[tauri::command]
fn list_capture_devices() -> Vec<DeviceDto> {
    let mut out: Vec<DeviceDto> = Vec::new();
    for d in list_loopback_devices().unwrap_or_default() {
        out.push(DeviceDto {
            name: d.name,
            sample_rate: d.sample_rate,
            channels: d.channels,
            is_default: d.is_default,
        });
    }
    for d in list_input_devices().unwrap_or_default() {
        out.push(DeviceDto {
            name: d.name,
            sample_rate: d.sample_rate,
            channels: d.channels,
            is_default: d.is_default,
        });
    }
    out
}

#[tauri::command]
fn list_output_devices_cmd() -> Vec<DeviceDto> {
    list_output_devices()
        .unwrap_or_default()
        .into_iter()
        .map(|d| DeviceDto {
            name: d.name,
            sample_rate: d.sample_rate,
            channels: d.channels,
            is_default: d.is_default,
        })
        .collect()
}

#[tauri::command]
fn lan_address_list() -> Vec<String> {
    lan_addresses().into_iter().map(|a| a.to_string()).collect()
}

#[tauri::command]
async fn discover_servers(timeout_ms: u64) -> Result<Vec<DiscoveredDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        ash_core::client::discover_servers(Duration::from_millis(timeout_ms))
            .map(|v| {
                v.into_iter()
                    .map(|d| DiscoveredDto {
                        name: d.name,
                        host: d.host,
                        port: d.port,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn server_start(
    state: EngineState<'_>,
    opts: ServerOptionsDto,
) -> Result<ServerStatsDto, String> {
    let encoding = match opts.encoding.as_str() {
        "f32" => Encoding::F32Le,
        _ => Encoding::S16Le,
    };
    let host: IpAddr = opts
        .host
        .as_deref()
        .unwrap_or("0.0.0.0")
        .parse()
        .map_err(|e| format!("bad host: {e}"))?;
    let source = match opts.source.as_str() {
        "mic" => SourceSelection::Input {
            device: opts.device.clone(),
        },
        "sine" => SourceSelection::Sine {
            sample_rate: 48000,
            channels: 2,
        },
        _ => SourceSelection::Loopback {
            device: opts.device.clone(),
        },
    };
    let cfg = ServerConfig {
        host,
        port: opts.port,
        source,
        encoding,
        block_ms: opts.block_ms.clamp(5, 200),
        name: if opts.name.trim().is_empty() {
            "AudioShare-RS".into()
        } else {
            opts.name.trim().into()
        },
        mdns: opts.mdns,
    };
    let handle = ServerHandle::start(cfg)
        .await
        .map_err(|e| format!("server start failed: {e:#}"))?;
    let dto = ServerStatsDto::from(handle.stats().as_ref());
    // Take over the engine slot, stopping any previous server/client.
    let (old_server, old_client) = {
        let mut eng = state.lock().unwrap();
        (eng.server.take(), eng.client.take())
    };
    if let Some(mut s) = old_server {
        s.stop().await;
    }
    if let Some(mut c) = old_client {
        c.stop().await;
    }
    {
        let mut eng = state.lock().unwrap();
        eng.server = Some(handle);
    }
    Ok(dto)
}

#[tauri::command]
async fn server_stop(state: EngineState<'_>) -> Result<(), String> {
    let taken = {
        let mut eng = state.lock().unwrap();
        eng.server.take()
    };
    if let Some(mut s) = taken {
        s.stop().await;
    }
    Ok(())
}

#[tauri::command]
fn server_stats(state: EngineState<'_>) -> ServerStatsDto {
    let eng = state.lock().unwrap();
    match &eng.server {
        Some(s) => ServerStatsDto::from(s.stats().as_ref()),
        None => idle_server_dto(),
    }
}

#[tauri::command]
async fn client_start(
    state: EngineState<'_>,
    opts: ClientOptionsDto,
) -> Result<ClientStatsDto, String> {
    let cfg = ClientConfig {
        host: opts.host.clone(),
        port: opts.port,
        tcp_data: opts.tcp_data,
        retry: true,
        sink_factory: Some(Box::new(CpalSinkFactory {
            device: opts.output_device.clone(),
            buffer_ms: opts.buffer_ms.clamp(20, 2000),
            volume: opts.volume.clamp(0.0, 2.0),
        })),
    };
    let handle = run_client(cfg)
        .await
        .map_err(|e| format!("client start failed: {e:#}"))?;
    let dto = ClientStatsDto::from(handle.stats().as_ref());
    // Take the old client out of the engine, then stop it outside the lock.
    let old = {
        let mut eng = state.lock().unwrap();
        eng.client.take()
    };
    if let Some(mut c) = old {
        c.stop().await;
    }
    {
        let mut eng = state.lock().unwrap();
        eng.client = Some(handle);
    }
    Ok(dto)
}

#[tauri::command]
async fn client_stop(state: EngineState<'_>) -> Result<(), String> {
    let taken = {
        let mut eng = state.lock().unwrap();
        eng.client.take()
    };
    if let Some(mut c) = taken {
        c.stop().await;
    }
    Ok(())
}

#[tauri::command]
fn client_stats(state: EngineState<'_>) -> ClientStatsDto {
    let eng = state.lock().unwrap();
    match &eng.client {
        Some(c) => ClientStatsDto::from(c.stats().as_ref()),
        None => idle_client_dto(),
    }
}

// ---------- ticker ----------

fn tick(app: &AppHandle) {
    let state: EngineState = app.state();
    let eng = state.lock().unwrap();
    let server = match &eng.server {
        Some(s) => ServerStatsDto::from(s.stats().as_ref()),
        None => idle_server_dto(),
    };
    let client = match &eng.client {
        Some(c) => ClientStatsDto::from(c.stats().as_ref()),
        None => idle_client_dto(),
    };
    drop(eng);
    let _ = app.emit("tick", TickPayload { server, client });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(Engine::default()))
        .invoke_handler(tauri::generate_handler![
            list_capture_devices,
            list_output_devices_cmd,
            lan_address_list,
            discover_servers,
            server_start,
            server_stop,
            server_stats,
            client_start,
            client_stop,
            client_stats
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut tick_i = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tick_i.tick().await;
                    tick(&handle);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running AudioShare Companion");
}
