//! `ashr` — the Audio Share Rust CLI.
//!
//! - `ashr serve`    : share this computer's audio on the LAN (the laptop side)
//! - `ashr play`     : connect to a server and play its audio (any computer)
//! - `ashr devices`  : list capture/loopback/playback devices
//! - `ashr addresses`: list LAN addresses the server would advertise

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use ash_core::capture::{list_input_devices, list_loopback_devices, SourceSelection};
use ash_core::client::{run_client, ClientConfig};
use ash_core::net::{default_lan_address, lan_addresses};
use ash_core::protocol::{Encoding, DEFAULT_PORT};
use ash_core::server::{ServerConfig, ServerHandle};

#[derive(Parser)]
#[command(
    name = "ashr",
    version,
    about = "Audio Share Rust — share your computer's audio over the LAN",
    long_about = "Recreation of mkckr0/audio-share in Rust.\n\n\
        `ashr serve` on the laptop shares its system audio;\n\
        `ashr play` (or the Android app / desktop companion) receives it."
)]
struct Cli {
    /// Increase verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum EncodingArg {
    /// 16-bit signed PCM — low bandwidth
    S16,
    /// 32-bit float PCM — bit-exact, higher bandwidth
    F32,
}

impl From<EncodingArg> for Encoding {
    fn from(v: EncodingArg) -> Self {
        match v {
            EncodingArg::S16 => Encoding::S16Le,
            EncodingArg::F32 => Encoding::F32Le,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum SourceArg {
    /// System loopback ("what you hear"); Windows uses WASAPI loopback,
    /// Linux/macOS use the default input (pick a monitor/BlackHole device).
    Loopback,
    /// Microphone or other input device.
    Mic,
    /// Built-in sine generator (demo / CI, no audio hardware).
    Sine,
}

#[derive(Subcommand)]
enum Command {
    /// Share this computer's audio on the LAN
    Serve {
        /// Bind address (default: first private LAN address)
        #[arg(short, long)]
        host: Option<IpAddr>,
        /// TCP + UDP port
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Audio source
        #[arg(short, long, value_enum, default_value_t = SourceArg::Loopback)]
        source: SourceArg,
        /// Device name substring (loopback endpoint on Windows, input device elsewhere)
        #[arg(short, long)]
        device: Option<String>,
        /// Wire encoding
        #[arg(short, long, value_enum, default_value_t = EncodingArg::S16)]
        encoding: EncodingArg,
        /// Capture chunk length in milliseconds
        #[arg(long, default_value_t = 20)]
        block_ms: u32,
        /// Friendly server name
        #[arg(short, long)]
        name: Option<String>,
        /// Disable mDNS announcement
        #[arg(long)]
        no_mdns: bool,
        /// Load settings from a TOML file first (CLI flags win)
        #[arg(long)]
        config: Option<String>,
    },
    /// Connect to a server and play its audio
    Play {
        /// Server host (IP or hostname)
        #[arg(short = 'H', long)]
        host: Option<String>,
        /// Server port
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Receive audio over TCP instead of UDP (for UDP-blocked networks)
        #[arg(long)]
        tcp_data: bool,
        /// Output device name substring (default: system output)
        #[arg(short, long)]
        output: Option<String>,
        /// Target jitter buffer in milliseconds
        #[arg(long, default_value_t = 120)]
        buffer_ms: u32,
        /// Output volume (0.0 – 2.0)
        #[arg(short, long, default_value_t = 1.0)]
        volume: f32,
        /// Give up after the first connection failure instead of retrying
        #[arg(long)]
        no_retry: bool,
        /// Load settings from a TOML file first (CLI flags win)
        #[arg(long)]
        config: Option<String>,
    },
    /// List capture/loopback/playback devices
    Devices,
    /// List LAN addresses clients can connect to
    Addresses,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = match cli.verbose {
        0 => "info",
        1 => "info,ash=info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter)?)
        .init();

    match cli.command {
        Command::Serve {
            host,
            port,
            source,
            device,
            encoding,
            block_ms,
            name,
            no_mdns,
            config,
        } => {
            let mut cfg = load_server_config(config.as_deref())?;
            cfg.host = host.unwrap_or("0.0.0.0".parse().unwrap());
            cfg.port = port;
            cfg.source = match source {
                SourceArg::Loopback => SourceSelection::Loopback { device },
                SourceArg::Mic => SourceSelection::Input { device },
                SourceArg::Sine => SourceSelection::Sine {
                    sample_rate: 48000,
                    channels: 2,
                },
            };
            cfg.encoding = encoding.into();
            cfg.block_ms = block_ms;
            if let Some(n) = name {
                cfg.name = n;
            }
            cfg.mdns = !no_mdns;

            let mut server = ServerHandle::start(cfg).await?;
            let s = server.stats();
            println!("server '{}' running", s.name);
            println!("  format: {}", s.format);
            println!("  listen: {} (TCP control + UDP data)", s.bind_addr);
            if let Some(ip) = default_lan_address() {
                println!("  phones/players connect to: {}:{}", ip, s.port);
            }
            println!("press Ctrl-C to stop");
            wait_for_ctrl_c().await;
            server.stop().await;
            Ok(())
        }
        Command::Play {
            host,
            port,
            tcp_data,
            output,
            buffer_ms,
            volume,
            no_retry,
            config,
        } => {
            let file = load_client_config(config.as_deref())?;
            let host: String = host
                .or(file.host)
                .with_context(|| "no host given — use --host <ip>")?;
            let buffer_ms = buffer_ms.max(if buffer_ms == 120 {
                file.buffer_ms.unwrap_or(120)
            } else {
                buffer_ms
            });
            let volume = if volume == 1.0 {
                file.volume.unwrap_or(volume)
            } else {
                volume
            };
            let tcp_data = tcp_data || file.tcp_data.unwrap_or(false);
            let ccfg = ClientConfig {
                host,
                port: file.port.unwrap_or(port),
                tcp_data,
                retry: !no_retry,
                sink_factory: Some(Box::new(ash_core::client::sink::CpalSinkFactory {
                    device: output,
                    buffer_ms,
                    volume: volume.clamp(0.0, 2.0),
                })),
            };

            let mut client = run_client(ccfg).await?;
            println!("connecting… press Ctrl-C to stop");
            let mut last = 0u64;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {
                        let s = client.stats();
                        if s.connected {
                            println!(
                                "buffered {} ms | received {:.1} MB | lost {} pkts{}",
                                s.buffered_ms,
                                s.bytes_received as f64 / 1e6,
                                s.lost_packets,
                                s.rtt_ms.map(|r| format!(" | rtt {r} ms")).unwrap_or_default(),
                            );
                            last = s.bytes_received;
                        } else if last > 0 {
                            println!("disconnected: {}", s.last_error.as_deref().unwrap_or("?"));
                            last = 0;
                        }
                    }
                }
            }
            client.stop().await;
            Ok(())
        }
        Command::Devices => {
            let inputs = list_input_devices().unwrap_or_default();
            let loopbacks = list_loopback_devices().unwrap_or_default();
            println!("loopback-capable endpoints (Windows):");
            for d in &loopbacks {
                println!(
                    "  {}{}",
                    d.name,
                    if d.is_default { "  [default]" } else { "" }
                );
            }
            if loopbacks.is_empty() {
                println!("  (n/a on this platform — use a monitor source / virtual device)");
            }
            println!("\ninput devices:");
            for d in &inputs {
                println!(
                    "  {}  ({} Hz, {} ch){}",
                    d.name,
                    d.sample_rate,
                    d.channels,
                    if d.is_default { "  [default]" } else { "" }
                );
            }
            Ok(())
        }
        Command::Addresses => {
            for a in lan_addresses() {
                println!("{a}");
            }
            Ok(())
        }
    }
}

async fn wait_for_ctrl_c() {
    tokio::signal::ctrl_c().await.ok();
}

// --- minimal TOML config files -------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
struct FileServer {
    host: Option<String>,
    port: Option<u16>,
    encoding: Option<String>,
    block_ms: Option<u32>,
    name: Option<String>,
    no_mdns: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileClient {
    host: Option<String>,
    port: Option<u16>,
    tcp_data: Option<bool>,
    buffer_ms: Option<u32>,
    volume: Option<f32>,
}

fn load_server_config(path: Option<&str>) -> Result<ServerConfig> {
    let mut cfg = ServerConfig::default();
    let Some(path) = path else { return Ok(cfg) };
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let f: FileServer = toml::from_str(&raw)?;
    if let Some(h) = f.host {
        cfg.host = h.parse()?;
    }
    if let Some(p) = f.port {
        cfg.port = p;
    }
    if let Some(e) = f.encoding.and_then(|s| Encoding::parse_label(&s)) {
        cfg.encoding = e;
    }
    if let Some(b) = f.block_ms {
        cfg.block_ms = b;
    }
    if let Some(n) = f.name {
        cfg.name = n;
    }
    if let Some(false) = f.no_mdns {
        cfg.mdns = false;
    }
    Ok(cfg)
}

fn load_client_config(path: Option<&str>) -> Result<FileClient> {
    let Some(path) = path else {
        return Ok(FileClient::default());
    };
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    Ok(toml::from_str(&raw)?)
}
