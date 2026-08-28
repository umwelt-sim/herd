//! One edge, relaying between game clients and regions.
//!
//! This is what a game developer's edge binary looks like: it builds a QUIC
//! endpoint, hands it to an [`EdgeServer`], and implements
//! [`EdgeGame`](umwelt::EdgeGame). The library does the rest — a client's
//! spawns, moves and despawns are relayed without this file being asked, and a
//! region's packets reach the right connection without being decoded on the way
//! through.
//!
//! ```text
//! cargo run --release -p herd-edge
//! cargo run --release -p herd-edge -- --edge 0.0.0.0:7777
//! ```
//!
//! Needs a `herd-sim` behind it and `herd-game` in front.
//!
//! # Where things are
//!
//! - [`game`] is what this edge does of its own accord, which is almost
//!   nothing.
//! - [`quic`] is the endpoint it listens on.

mod game;
mod quic;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::EdgeServer;

/// Everything the command line decides.
struct Options {
    /// Where the broker is.
    nats: String,
    /// Where this edge listens for game clients.
    listen: String,
    /// Seconds between heartbeats. Zero switches them off.
    heartbeat: u64,
}

impl Options {
    fn from_args() -> Options {
        Options {
            nats: herd_common::arg_or("nats", herd_common::DEFAULT_NATS.to_string()),
            listen: herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string()),
            heartbeat: herd_common::arg_or("heartbeat", 30u64),
        }
    }
}

fn main() {
    let options = Options::from_args();

    // This binary owns both ends. Where the broker is, what certificate the
    // edge presents, and which crypto provider is installed are all decided
    // here rather than by the library; see docs/adr/0006.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats = runtime
        .block_on(herd_common::connect(&options.nats, herd_common::arg("creds")))
        .unwrap_or_else(|e| {
            eprintln!("nats {}: {e}", options.nats);
            std::process::exit(1);
        });
    let quic = quic::endpoint(&options.listen, runtime.handle());

    let clients = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let clients = Arc::clone(&clients);
        EdgeServer::new(nats, runtime.handle().clone(), quic, move |_handle| game::Game {
            clients,
        })
    }
    .unwrap_or_else(|e| {
        eprintln!("starting the edge: {e}");
        std::process::exit(1);
    });
    server.set_heartbeat_interval(Duration::from_secs(options.heartbeat));
    println!("herd-edge: {} listening on {}", server.name(), options.listen);

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
