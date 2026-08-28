//! A game, on the other side of an edge.
//!
//! This is what a game developer writes: it holds no region connection, speaks
//! no NATS, frames no messages, chooses no transports and polls nothing. It
//! asks the edge for a population, walks it, and hands parts of it back as
//! clients would come and go. What the edge says arrives as calls on a
//! `ClientGame`, and whether a command rides a datagram or a reliable stream is
//! a property of the command.
//!
//! ```text
//! cargo run --release -p herd-game
//! cargo run --release -p herd-game -- --observers 512 --churn 8
//! cargo run --release -p herd-game -- --clients 4 --observers 128
//! ```
//!
//! Needs a `herd-edge` listening, which needs a `herd-sim` behind it.
//!
//! `--clients` opens that many connections from this one process, each with its
//! own population, which is how one machine stands in for a crowd.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use umwelt::net::{ClientHandle, EdgeClient, EntityKind, RegionId};
use umwelt::{ClientGame, EntityId, Fixed, PacketReader, Pos3, RecordCodec, WorldConfig};

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

/// What this game does with what its edge says.
///
/// The whole of a consumer's receive side: no polling, no timeout, and no
/// decision about what silence means.
struct Watcher {
    spawned: Arc<Mutex<Vec<(u32, RegionId, EntityId)>>>,
    removed: Arc<Mutex<Vec<u32>>>,
    packets: Arc<AtomicU64>,
    records: Arc<AtomicU64>,
    gone: Arc<AtomicBool>,
    codec: RecordCodec,
}

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: u32, region: RegionId, entity: EntityId) {
        self.spawned.lock().expect("not poisoned").push((handle, region, entity));
    }

    fn removed(&mut self, handle: u32) {
        self.removed.lock().expect("not poisoned").push(handle);
    }

    fn state(&mut self, _region: RegionId, packet: &[u8]) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        if let Some(reader) = PacketReader::new(&self.codec, packet) {
            self.records.fetch_add(reader.updates().count() as u64, Ordering::Relaxed);
        }
    }

    fn disconnected(&mut self) {
        self.gone.store(true, Ordering::Relaxed);
    }
}

/// One entity this client asked for, named by the handle the edge gave back.
struct Held {
    handle: u32,
    at: Pos3,
    heading: i32,
    /// The region's own id, once the edge has said what it is. Until then the
    /// handle is the only name for it, which is the point of having one.
    entity: Option<(RegionId, EntityId)>,
}

fn main() {
    let addr: String = herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string());
    let clients: usize = herd_common::arg_or("clients", 1usize);
    let observers: usize = herd_common::arg_or("observers", 512usize);
    let unattended: usize = herd_common::arg_or("unattended", 0usize);
    let churn: usize = herd_common::arg_or("churn", 0usize);
    // Which region this game puts its players in. The edge has no home and no
    // opinion: the map of regions is the game's, kept out of band, so the
    // client is what names one. See docs/adr/0003.
    let region = RegionId::from_raw(herd_common::arg_or("region", 7u32));
    let cfg = herd_common::world(herd_common::arg_or("hz", 20u32));

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let endpoint = herd_common::game_endpoint(runtime.handle());
    println!(
        "herd-game: {clients} clients to {addr}, {observers} observers each in {region}"
    );

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for n in 0..clients {
            let endpoint = endpoint.clone();
            let runtime = runtime.handle().clone();
            let addr = addr.clone();
            let stop = &stop;
            scope.spawn(move || {
                play(
                    &runtime, &endpoint, &addr, n, region, cfg, observers, unattended,
                    churn, stop,
                )
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn play(
    runtime: &tokio::runtime::Handle,
    endpoint: &quinn::Endpoint,
    addr: &str,
    n: usize,
    region: RegionId,
    cfg: WorldConfig,
    observers: usize,
    unattended: usize,
    churn: usize,
    stop: &AtomicBool,
) {
    let target = addr.parse().unwrap_or_else(|e| {
        eprintln!("--edge {addr:?}: {e}");
        std::process::exit(1);
    });
    // This binary owns its connection, so where the edge is and what it has to
    // present to be believed are decided here rather than by umwelt.
    let conn = runtime
        .block_on(async {
            match endpoint.connect(target, "localhost") {
                Ok(connecting) => connecting.await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        })
        .unwrap_or_else(|e| {
            eprintln!("connecting to {addr}: {e}");
            std::process::exit(1);
        });
    let codec = RecordCodec::new(&cfg);
    let hz = cfg.tick_hz();

    // What the edge has said. One place, whichever transport carried it.
    let spawned: Arc<Mutex<Vec<(u32, RegionId, EntityId)>>> = Arc::new(Mutex::new(Vec::new()));
    let removed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let packets = Arc::new(AtomicU64::new(0));
    let records = Arc::new(AtomicU64::new(0));
    let gone = Arc::new(AtomicBool::new(false));
    let watcher = Watcher {
        spawned: Arc::clone(&spawned),
        removed: Arc::clone(&removed),
        packets: Arc::clone(&packets),
        records: Arc::clone(&records),
        gone: Arc::clone(&gone),
        codec,
    };
    let client = EdgeClient::new(conn, runtime.clone(), |_handle| watcher).unwrap_or_else(|e| {
        eprintln!("opening a stream: {e}");
        std::process::exit(1);
    });
    let sending: ClientHandle = client.handle();

    // A column of its own per client, so several do not stack on one spot.
    let lane = ((std::process::id() as usize + n * 7) % 64) as i32 * 60 + 64;
    let home = |k: usize| Pos3::from_meters(lane, 64 + (k as i32 % 3072), 0);

    let mut held: Vec<Held> = Vec::new();
    for k in 0..observers + unattended {
        let kind = if k < observers { EntityKind::Observer } else { EntityKind::Unattended };
        let at = home(k);
        match sending.spawn(region, at, kind) {
            Ok(handle) => held.push(Held { handle, at, heading: 1, entity: None }),
            Err(e) => {
                eprintln!("asking for an entity: {e}");
                std::process::exit(1);
            }
        }
    }

    {
        let period = Duration::from_millis(1_000 / hz.max(1) as u64);
        let step = Fixed::from_raw(Fixed::from_meters(WALK_M_PER_SEC).raw() / hz.max(1) as i32);
        let mut reported = Instant::now();
        let mut sent = 0u64;
        let mut given_back = 0u64;
        let mut next_home = observers + unattended;

        while !stop.load(Ordering::Relaxed) && !gone.load(Ordering::Relaxed) {
            let deadline = Instant::now() + period;

            for (handle, region, entity) in spawned.lock().expect("not poisoned").drain(..) {
                if let Some(one) = held.iter_mut().find(|h| h.handle == handle) {
                    one.entity = Some((region, entity));
                }
            }
            // Anything the edge says is gone stops being moved, however it went
            // — including a despawn this client never asked for.
            for handle in removed.lock().expect("not poisoned").drain(..) {
                held.retain(|h| h.handle != handle);
            }

            for one in held.iter_mut() {
                let moved = Fixed::from_raw(one.at.x.raw() + step.raw() * one.heading);
                if (moved.floor_meters() - lane).abs() > RANGE_M {
                    one.heading = -one.heading;
                } else {
                    one.at.x = moved;
                }
            }
            // A handle names an entity from the moment it is asked for, so a
            // move can be sent before the region has answered.
            let moves: Vec<(u32, Pos3)> = held.iter().map(|h| (h.handle, h.at)).collect();
            if !moves.is_empty() {
                if sending.move_entities(&moves).is_err() {
                    return;
                }
                sent += moves.len() as u64;
            }

            if reported.elapsed() >= Duration::from_secs(1) {
                if churn > 0 && held.len() >= churn {
                    let leaving: Vec<u32> =
                        held.iter().rev().take(churn).map(|h| h.handle).collect();
                    for handle in &leaving {
                        if sending.despawn(*handle).is_err() {
                            return;
                        }
                    }
                    given_back += leaving.len() as u64;
                    // Replacements. The edge mints each handle, spent once and
                    // never reused.
                    for _ in 0..leaving.len() {
                        let at = home(next_home);
                        next_home += 1;
                        match sending.spawn(region, at, EntityKind::Observer) {
                            Ok(handle) => {
                                held.push(Held { handle, at, heading: 1, entity: None })
                            }
                            Err(_) => return,
                        }
                    }
                }

                println!(
                    "herd-game[{n}]: holding {} ({} with ids) | {sent} moves sent | \
                     {} packets | {} records | {given_back} handed back",
                    held.len(),
                    held.iter().filter(|h| h.entity.is_some()).count(),
                    packets.swap(0, Ordering::Relaxed),
                    records.swap(0, Ordering::Relaxed),
                );
                reported = Instant::now();
                sent = 0;
            }

            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
    }
}
