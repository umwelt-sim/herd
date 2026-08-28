//! Herd is an example of a game that could have been created by a game
//! developer using the Umwelt library. It is designed as a smoke tester
//! and load generator, not a playable thing.
//!
//! ```text
//! cargo run --release -p herd-game
//! cargo run --release -p herd-game -- --observers 512 --churn 8
//! cargo run --release -p herd-game -- --clients 4 --observers 128
//! cargo run --release -p herd-game -- --to 8 --migrate 32
//! ```
//!
//! The last line needs a second `herd-sim --region 8` as well. It walks part of
//! the crowd between the two regions by the sequence in `docs/adr/0003`, which
//! is a game's to perform: umwelt does not know which regions exist, and the
//! map of them is the game's, kept out of band.
//!
//! Needs a `herd-edge` listening, which needs a `herd-sim` behind it.
//!
//! `--clients` opens that many connections from this one process, each with its
//! own population, which is how one machine stands in for a crowd.
//!
//! # Where things are
//!
//! - [`watcher`] is what the edge tells this game: umwelt calls it.
//! - [`crowd`] is the game itself — entities, where they are, where they walk.
//! - [`link`] is the four calls the crowd makes into umwelt.
//! - [`session`] is one connection and the loop that drives it.
//!
//! Nothing here frames a message, picks a transport, or polls for one.

mod crowd;
mod link;
mod session;
mod watcher;

use std::sync::atomic::AtomicBool;

use umwelt::RegionId;

/// Everything the command line decides.
pub struct Options {
    /// Where the edge is listening.
    pub addr: String,
    /// Connections to open from this one process.
    pub clients: usize,
    /// Entities with a game client behind them. Each costs a viewer.
    pub observers: usize,
    /// Entities with nothing behind them: replicated to whoever can see them,
    /// and sent nothing themselves.
    pub unattended: usize,
    /// Observers to hand back and replace each second, standing in for game
    /// clients disconnecting and connecting.
    pub churn: usize,
    /// Which region this game puts its players in. Note that this doesn't
    /// couple a game to a region. A real game would be aware of multiple
    /// regions and be able to spawn in all of them.
    pub region: RegionId,
    /// Another region to walk part of the crowd into, and back from. It has to
    /// be running: a game keeps the map of regions, and umwelt does not know
    /// which exist.
    pub to: Option<RegionId>,
    /// Entities to walk into the other region each second.
    pub migrate: usize,
    /// How often this client sends. A real game might not have a
    /// fixed send rate. This binary does because it's simulating player
    /// interaction (for now).
    pub send_hz: u32,
}

impl Options {
    fn from_args() -> Options {
        Options {
            addr: herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string()),
            clients: herd_common::arg_or("clients", 1usize),
            observers: herd_common::arg_or("observers", 512usize),
            unattended: herd_common::arg_or("unattended", 0usize),
            churn: herd_common::arg_or("churn", 0usize),
            region: RegionId::from_raw(herd_common::arg_or("region", 7u32)),
            to: herd_common::arg("to").map(|raw| {
                RegionId::from_raw(raw.parse().unwrap_or_else(|_| {
                    eprintln!("--to: cannot read {raw:?}");
                    std::process::exit(2);
                }))
            }),
            migrate: herd_common::arg_or("migrate", 0usize),
            send_hz: herd_common::arg_or("send-hz", 20u32),
        }
    }
}

fn main() {
    let options = Options::from_args();
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let endpoint = herd_common::game_endpoint(runtime.handle());
    println!(
        "herd-game: {} clients to {}, {} observers each in {}",
        options.clients, options.addr, options.observers, options.region
    );

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for n in 0..options.clients {
            let endpoint = endpoint.clone();
            let runtime = runtime.handle().clone();
            let options = &options;
            let stop = &stop;
            scope.spawn(move || session::play(&runtime, &endpoint, options, n, stop));
        }
    });
}
