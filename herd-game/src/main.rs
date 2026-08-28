//! A game, on the other side of an edge.
//!
//! This is what a game developer writes: it holds no region connection, speaks
//! no NATS, and knows nothing about regions except the ids that come back on
//! its own entities. It asks the edge for a population, walks it, hands parts
//! of it back as clients would come and go, and reads the replication that
//! comes down.
//!
//! ```text
//! cargo run --release --example herd-game
//! cargo run --release --example herd-game -- --observers 512 --churn 8
//! cargo run --release --example herd-game -- --clients 4 --observers 128
//! ```
//!
//! Needs a `herd-edge` listening, which needs a `herd-sim` behind it.
//!
//! `--clients` opens that many QUIC connections from this one process, each
//! with its own population, which is how one machine stands in for a crowd.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{EntityKind, Framer, FromClient, ToClient};
use umwelt::{EntityId, Fixed, PacketReader, Pos3, RecordCodec, RegionId, WorldConfig};

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

/// One entity this client asked for, named by the handle it chose.
struct Held {
    handle: u32,
    at: Pos3,
    heading: i32,
    observes: bool,
    /// The region's own id, once the edge has said what it is. Until then the
    /// handle is the only name for it, which is the point of having one.
    entity: Option<(RegionId, EntityId)>,
}

fn main() {
    let addr: String = herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string());
    // Which region this game puts its players in. The edge has no home and no
    // opinion: the map of regions is the game's, kept out of band, so the
    // client is what names one. See docs/adr/0003.
    let region = RegionId::from_raw(herd_common::arg_or("region", 7u32));
    let clients: usize = herd_common::arg_or("clients", 1usize);
    let observers: usize = herd_common::arg_or("observers", 512usize);
    let unattended: usize = herd_common::arg_or("unattended", 0usize);
    let churn: usize = herd_common::arg_or("churn", 0usize);
    // The world this game was built against. The edge checked the region's
    // digest when it started, so a mismatch here shows up as packets that do
    // not decode rather than as a handshake failure.
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
    // One bidirectional stream carries everything reliable, both ways. The
    // client opens it; the edge accepts it.
    let (send, recv) = runtime
        .block_on(conn.open_bi())
        .unwrap_or_else(|e| {
            eprintln!("opening a stream: {e}");
            std::process::exit(1);
        });

    let codec = RecordCodec::new(&cfg);
    let hz = cfg.tick_hz();
    // A column of its own per client, so several do not stack on one spot.
    let lane = ((std::process::id() as usize + n * 7) % 64) as i32 * 60 + 64;
    let home = |k: usize| Pos3::from_meters(lane, 64 + (k as i32 % 3072), 0);

    // What the edge has said about entities this client asked for.
    let spawned: Mutex<Vec<(u32, RegionId, EntityId)>> = Mutex::new(Vec::new());
    let removed: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let updates = AtomicU64::new(0);
    let records = AtomicU64::new(0);
    let own = AtomicU64::new(0);

    std::thread::scope(|scope| {
        // Reliable, coming down: which ids the regions allocated, and what has
        // gone. A stream has no message boundaries, so it is framed.
        scope.spawn(|| {
            let mut recv = recv;
            let mut framer = Framer::new();
            let mut buf = vec![0u8; 16 * 1024];
            while !stop.load(Ordering::Relaxed) {
                let read = match runtime.block_on(recv.read(&mut buf)) {
                    Ok(Some(read)) => read,
                    _ => return,
                };
                framer.push(&buf[..read]);
                while let Ok(Some(body)) = framer.take() {
                    match ToClient::decode(&body) {
                        Ok(ToClient::Spawned { handle, region, entity }) => {
                            spawned.lock().expect("not poisoned").push((
                                handle, region, entity,
                            ));
                        }
                        Ok(ToClient::Removed { handle }) => {
                            removed.lock().expect("not poisoned").push(handle);
                        }
                        _ => {}
                    }
                }
            }
        });

        // Datagrams, coming down: the replication itself.
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let Ok(datagram) = runtime.block_on(conn.read_datagram()) else { return };
                let Ok(ToClient::State { packet, .. }) = ToClient::decode(&datagram) else {
                    continue;
                };
                updates.fetch_add(1, Ordering::Relaxed);
                let Some(reader) = PacketReader::new(&codec, packet) else { continue };
                records.fetch_add(reader.updates().count() as u64, Ordering::Relaxed);
                own.fetch_add(reader.despawns().count() as u64, Ordering::Relaxed);
            }
        });

        // Up: ask for a population, then walk it.
        let mut send = send;
        let mut body = Vec::new();
        let mut framed = Vec::new();
        let post = |message: &FromClient,
                        send: &mut quinn::SendStream,
                        body: &mut Vec<u8>,
                        framed: &mut Vec<u8>|
         -> bool {
            message.encode(body);
            if message.is_latest_only() {
                // Latest-only, so it rides a datagram: a lost one is superseded
                // within a tick.
                conn.send_datagram(body.clone().into()).is_ok()
            } else {
                Framer::frame(body, framed);
                runtime.block_on(send.write_all(framed)).is_ok()
            }
        };

        let mut held: Vec<Held> = Vec::new();
        let mut next_handle = 0u32;
        for k in 0..observers + unattended {
            let kind =
                if k < observers { EntityKind::Observer } else { EntityKind::Unattended };
            let handle = next_handle;
            next_handle += 1;
            let at = home(k);
            held.push(Held {
                handle,
                at,
                heading: 1,
                observes: kind.observes(),
                entity: None,
            });
            let ask = FromClient::Spawn { handle, region, position: at, kind };
            if !post(&ask, &mut send, &mut body, &mut framed) {
                return;
            }
        }

        let period = Duration::from_millis(1_000 / hz.max(1) as u64);
        let step = Fixed::from_raw(Fixed::from_meters(WALK_M_PER_SEC).raw() / hz.max(1) as i32);
        let mut reported = Instant::now();
        let mut sent = 0u64;
        let mut given_back = 0u64;
        let mut next_home = observers + unattended;

        while !stop.load(Ordering::Relaxed) {
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
                // A handle names an entity from the moment it is asked for, so
                // a move can be sent before the region has answered.
                let go = FromClient::Move { handle: one.handle, position: one.at };
                if post(&go, &mut send, &mut body, &mut framed) {
                    sent += 1;
                }
            }

            if reported.elapsed() >= Duration::from_secs(1) {
                if churn > 0 {
                    let leaving: Vec<u32> = held
                        .iter()
                        .filter(|h| h.observes)
                        .rev()
                        .take(churn)
                        .map(|h| h.handle)
                        .collect();
                    for handle in &leaving {
                        let go = FromClient::Despawn { handle: *handle };
                        if !post(&go, &mut send, &mut body, &mut framed) {
                            return;
                        }
                    }
                    given_back += leaving.len() as u64;
                    // Replacements, each under a handle spent once and never
                    // reused.
                    for _ in 0..leaving.len() {
                        let handle = next_handle;
                        next_handle += 1;
                        let at = home(next_home);
                        next_home += 1;
                        held.push(Held {
                            handle,
                            at,
                            heading: 1,
                            observes: true,
                            entity: None,
                        });
                        let ask = FromClient::Spawn {
                            handle,
                            region,
                            position: at,
                            kind: EntityKind::Observer,
                        };
                        if !post(&ask, &mut send, &mut body, &mut framed) {
                            return;
                        }
                    }
                }

                println!(
                    "herd-game[{n}]: holding {} ({} with ids) | {sent} moves sent | \
                     {} packets | {} records | {given_back} handed back",
                    held.len(),
                    held.iter().filter(|h| h.entity.is_some()).count(),
                    updates.swap(0, Ordering::Relaxed),
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
    });
}
