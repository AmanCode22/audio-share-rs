//! End-to-end tests: a full server ↔ client round trip on localhost using
//! the sine source and the NullSink — no audio hardware required.

use std::sync::Once;
use std::time::Duration;

static INIT: Once = Once::new();
fn init_log() {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .try_init();
    });
}

use ash_core::capture::SourceSelection;
use ash_core::client::{run_client, ClientConfig};
use ash_core::protocol::Encoding;
use ash_core::server::{ServerConfig, ServerHandle};

fn random_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static N: AtomicU16 = AtomicU16::new(0);
    let base = 40000 + (std::process::id() % 20000) as u16;
    base + N.fetch_add(1, Ordering::Relaxed) * 7
}

async fn run_roundtrip(tcp_data: bool) {
    init_log();
    let mut server = ServerHandle::start(ServerConfig {
        host: "127.0.0.1".parse().unwrap(),
        port: random_port(), // concrete port: TCP and UDP share it
        source: SourceSelection::Sine {
            sample_rate: 48000,
            channels: 2,
        },
        encoding: Encoding::S16Le,
        block_ms: 20,
        name: "test-server".into(),
        mdns: false,
    })
    .await
    .expect("server start");

    let stats = server.stats();
    assert!(stats.running);
    assert_eq!(stats.format.channels, 2);
    assert_eq!(stats.format.sample_rate, 48000);
    let port = stats
        .bind_addr
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .expect("port from bind addr");

    let mut client = run_client(ClientConfig {
        host: "127.0.0.1".into(),
        port,
        tcp_data,
        retry: false,
        sink_factory: None, // NullSink
    })
    .await
    .expect("client start");

    // Give the pipeline a moment to move sine data through.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cs = client.stats();
        if cs.bytes_received > 48_000 * 2 * 2 {
            break;
        }
    }
    let cs = client.stats();
    assert!(cs.connected, "client should be connected");
    assert_eq!(cs.server_name, "test-server");
    assert!(
        cs.bytes_received > 48_000 * 2 * 2,
        "received {} bytes",
        cs.bytes_received
    );
    assert_eq!(cs.lost_packets, 0, "no loss on localhost");

    let peers = server.stats().clients.len();
    assert!(peers >= 1, "server should see at least one client");

    client.stop().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = server.stats();
    assert!(after.clients.is_empty(), "peer removed after stop");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roundtrip_udp() {
    run_roundtrip(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roundtrip_tcp() {
    run_roundtrip(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_and_resync() {
    init_log();
    // Server dies mid-stream; client with retry=false should surface an error.
    let mut server = ServerHandle::start(ServerConfig {
        host: "127.0.0.1".parse().unwrap(),
        port: random_port(),
        source: SourceSelection::Sine {
            sample_rate: 44100,
            channels: 1,
        },
        encoding: Encoding::S16Le,
        block_ms: 20,
        name: "dying".into(),
        mdns: false,
    })
    .await
    .expect("server start");
    let port = server
        .stats()
        .bind_addr
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();

    let mut client = run_client(ClientConfig {
        host: "127.0.0.1".into(),
        port,
        tcp_data: false,
        retry: false,
        sink_factory: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(client.stats().connected);

    server.stop().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let cs = client.stats();
    assert!(!cs.connected, "client should notice server death");
    assert!(cs.last_error.is_some());
    client.stop().await;
}
