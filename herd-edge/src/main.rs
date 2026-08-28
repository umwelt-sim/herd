//! One edge, relaying between game clients and regions.
//!
//! This is what a game developer's edge binary looks like: it builds a QUIC
//! endpoint, hands it to an [`EdgeServer`], and implements [`EdgeGame`]. The
//! library does the rest — a client's spawns, moves and despawns are relayed
//! without this file being asked, and a region's packets reach the right
//! connection without being decoded on the way through.
//!
//! ```text
//! cargo run --release -p herd-edge
//! cargo run --release -p herd-edge -- --edge 0.0.0.0:7777
//! ```
//!
//! Needs a `herd-sim` behind it and `herd-game` in front.
//!
//! It holds no entities of its own. An edge is a relay: what walks around a
//! region belongs to a game, and migrating between two of them is a game's to
//! perform — see `herd-game --to`. An edge that kept a herd would be a
//! simulation living in the wrong tier.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::{ClientId, EdgeGame, EdgeServer};

/// What this edge does of its own accord, which is almost nothing.
///
/// Every callback below has a default that does nothing, and a game whose
/// clients only spawn, move and despawn implements none of them. These two are
/// here to say who is connected.
struct Game {
    clients: Arc<Mutex<Vec<ClientId>>>,
}

impl EdgeGame for Game {
    fn connected(&mut self, client: ClientId, from: std::net::SocketAddr) {
        println!("herd-edge: {client} connected from {from}");
        self.clients.lock().expect("not poisoned").push(client);
    }

    fn disconnected(&mut self, client: ClientId) {
        // Everything this client held is already despawned and `removed` has
        // already fired for each, so there is nothing to clean up here except
        // what this file itself keeps.
        println!("herd-edge: {client} gone");
        self.clients.lock().expect("not poisoned").retain(|held| *held != client);
    }
}

fn main() {
    let url: String = herd_common::arg_or("nats", herd_common::DEFAULT_NATS.to_string());
    let listen: String = herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string());

    // This binary owns both ends. Where the broker is, what certificate the
    // edge presents, and which crypto provider is installed are all decided
    // here rather than by the library; see docs/adr/0006.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats = runtime
        .block_on(herd_common::connect(&url, herd_common::arg("creds")))
        .unwrap_or_else(|e| {
            eprintln!("nats {url}: {e}");
            std::process::exit(1);
        });
    let quic = edge_endpoint(&listen, runtime.handle());

    let clients = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let clients = Arc::clone(&clients);
        EdgeServer::new(nats, runtime.handle().clone(), quic, move |_handle| Game { clients })
    }
    .unwrap_or_else(|e| {
        eprintln!("starting the edge: {e}");
        std::process::exit(1);
    });
    server.set_heartbeat_interval(Duration::from_secs(herd_common::arg_or("heartbeat", 30u64)));
    println!("herd-edge: {} listening on {listen}", server.name());

    let stop = AtomicBool::new(false);
    let mut reported = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if reported.elapsed() < Duration::from_secs(1) {
            continue;
        }
        let stats = server.stats();
        println!(
            "herd-edge: {} clients | {} entities ({} observing) | \
             relayed {} undeliverable {} | commands {} refused {}",
            stats.clients,
            stats.entities,
            stats.observers,
            stats.relayed,
            stats.undeliverable,
            stats.commands,
            stats.refused,
        );
        reported = Instant::now();
    }
}

// -- the endpoint this edge listens on --------------------------------------

/// A listening endpoint with a certificate generated for this run.
///
/// Self-signed, which is right for a demo on one machine and wrong for
/// anything else. A deployment builds its endpoint from whatever its operator
/// actually trusts and hands that over instead.
fn edge_endpoint(addr: &str, runtime: &tokio::runtime::Handle) -> quinn::Endpoint {
    herd_common::provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .unwrap_or_else(|e| {
            eprintln!("generating a certificate: {e}");
            std::process::exit(1);
        });
    let chain = vec![cert.cert.der().clone()];
    let key = quinn::rustls::pki_types::PrivateKeyDer::try_from(
        cert.signing_key.serialize_der(),
    )
    .expect("a key rcgen just produced");

    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap_or_else(|e| {
            eprintln!("server tls: {e}");
            std::process::exit(1);
        });
    tls.alpn_protocols = vec![herd_common::ALPN.to_vec()];
    let tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .expect("a TLS 1.3 config");
    let config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(tls));

    let addr: std::net::SocketAddr = addr.parse().unwrap_or_else(|e| {
        eprintln!("--edge {addr:?}: {e}");
        std::process::exit(1);
    });
    // A quinn endpoint spawns its own driver, so it has to be built inside the
    // runtime that will carry it.
    let _guard = runtime.enter();
    quinn::Endpoint::server(config, addr).unwrap_or_else(|e| {
        eprintln!("binding {addr}: {e}");
        std::process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo TLS setup, which is easy to get wrong in a way that only shows
    /// up at connect time. No broker involved: this is the client link alone.
    #[test]
    fn a_game_endpoint_reaches_an_edge_endpoint() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge = edge_endpoint("127.0.0.1:0", runtime.handle());
        let at = edge.local_addr().expect("bound");
        let game = herd_common::game_endpoint(runtime.handle());

        runtime.block_on(async move {
            let served = tokio::spawn(async move {
                let conn = edge.accept().await.expect("a connection").await.expect("shakes");
                let (mut send, mut recv) = conn.accept_bi().await.expect("a stream");
                let mut got = [0u8; 5];
                recv.read_exact(&mut got).await.expect("five bytes");
                send.write_all(&got).await.expect("writes them back");
                // Held until the far end has read: dropping the connection, or
                // the endpoint behind it, closes it under the reader.
                tokio::time::sleep(Duration::from_millis(200)).await;
                got
            });

            let conn =
                game.connect(at, "localhost").expect("configured").await.expect("connects");
            let (mut send, mut recv) = conn.open_bi().await.expect("a stream");
            send.write_all(b"herd!").await.expect("writes");
            let mut back = [0u8; 5];
            recv.read_exact(&mut back).await.expect("five bytes back");
            assert_eq!(&back, b"herd!");
            assert_eq!(&served.await.expect("the edge side finished"), b"herd!");
        });
    }

    /// The one number that decides whether a region's payloads can reach a
    /// client at all, and the reason herd does not use umwelt's default.
    #[test]
    fn a_packet_at_herd_s_budget_fits_a_datagram() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge = edge_endpoint("127.0.0.1:0", runtime.handle());
        let at = edge.local_addr().expect("bound");
        let game = herd_common::game_endpoint(runtime.handle());

        runtime.block_on(async move {
            let listening = tokio::spawn(async move {
                let conn = edge.accept().await.expect("a connection").await.expect("shakes");
                conn.read_datagram().await.map(|d| d.len())
            });
            let conn =
                game.connect(at, "localhost").expect("configured").await.expect("connects");
            let full = herd_common::PAYLOAD_BYTES as usize + 5;
            let room = conn.max_datagram_size().expect("datagrams are enabled");
            assert!(
                full <= room,
                "a full packet plus its header is {full} bytes against {room} of room"
            );
            conn.send_datagram(vec![0u8; full].into()).expect("fits");
            assert_eq!(listening.await.expect("joined").expect("read"), full);

            // And umwelt's default does not, which is the whole reason
            // PAYLOAD_BYTES exists.
            let too_big = umwelt::ClientLimits::default().payload_bytes as usize + 5;
            assert!(too_big > room, "umwelt's default now fits; PAYLOAD_BYTES can go");
        });
    }
}
