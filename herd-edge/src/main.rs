//! One edge, relaying between game clients and regions.
//!
//! This is what a game developer's edge binary looks like: it builds a QUIC
//! endpoint, hands it to an [`EdgeServer`], and implements [`EdgeGame`]. The
//! library does the rest — a client's spawns, moves and despawns are relayed
//! without this file being asked, and a region's packets reach the right
//! connection without being decoded on the way through.
//!
//! ```text
//! cargo run --release --example herd-edge
//! cargo run --release --example herd-edge -- --edge 0.0.0.0:7777 --region 7
//! cargo run --release --example herd-edge -- --to 8 --migrate 32
//! ```
//!
//! Needs a `herd-sim` behind it, and `herd-game` in front. The third line needs
//! a second `herd-sim --region 8` as well: it walks a herd of its own between
//! the two regions by the sequence in `docs/adr/0003`, which is the whole of
//! what ad hoc migration is — no message the protocol does not already have.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{ClientId, EntityKey, EntityKind, RegionId};
use umwelt::{EdgeGame, EdgeHandle, EdgeServer, EntityId, Fixed, Pos3};

/// Meters per second a traveler covers, well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of its lane a traveler walks before turning around.
const RANGE_M: i32 = 32;

/// One entity this edge manages on nobody's behalf, and walks between regions.
///
/// Unattended rather than an observer: nothing is behind it, so a region sends
/// it nothing and there is no client for the edge to route a packet to.
struct Traveler {
    region: RegionId,
    at: Pos3,
    heading: i32,
}

/// The herd this edge walks back and forth, and what is mid-transition.
#[derive(Default)]
struct Herd {
    live: HashMap<EntityKey, Traveler>,
    /// Asked for in a destination, with the origin's copy still there. The
    /// wait belongs here rather than in the library: an add reaches an edge on
    /// the same channel as payloads, so waiting inside a `migrate` call would
    /// mean eating messages meant for its own loop. See `docs/adr/0003`.
    in_transit: HashMap<EntityKey, EntityKey>,
    migrated: u64,
}

/// What this edge does of its own accord.
///
/// Every callback below has a default that does nothing, and a game whose
/// clients only spawn, move and despawn needs none of them. These are here
/// because this edge keeps a herd of its own.
struct Game {
    handle: EdgeHandle,
    herd: Arc<Mutex<Herd>>,
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

    fn spawned(
        &mut self,
        entity: EntityKey,
        client: Option<ClientId>,
        region: RegionId,
        _id: EntityId,
    ) {
        if client.is_some() {
            return; // A client's own. The library has already told it.
        }
        let mut herd = self.herd.lock().expect("not poisoned");
        // Step three of docs/adr/0003: the destination has it, so the origin's
        // copy can go back, and only now. Ordered the other way there is a
        // window where the entity exists nowhere.
        let Some(was) = herd.in_transit.remove(&entity) else {
            // Not a migration, so this is one of the herd this edge asked for
            // at startup and already recorded where it put it. Overwriting that
            // would move it to the origin, which is nowhere near its lane.
            return;
        };
        let at = herd.live.remove(&was).map(|t| t.at).unwrap_or_default();
        herd.live.insert(entity, Traveler { region, at, heading: 1 });
        herd.migrated += 1;
        drop(herd);
        let _ = self.handle.despawn(was);
    }

    fn removed(&mut self, entity: EntityKey, _client: Option<ClientId>) {
        let mut herd = self.herd.lock().expect("not poisoned");
        herd.live.remove(&entity);
        herd.in_transit.remove(&entity);
    }
}

fn main() {
    let url: String = herd_common::arg_or("nats", herd_common::DEFAULT_NATS.to_string());
    let listen: String = herd_common::arg_or("edge", herd_common::DEFAULT_EDGE.to_string());
    let region = RegionId::from_raw(herd_common::arg_or("region", 7u32));
    // Where travelers walk to, and back from. A second region, which has to be
    // running: nothing here knows which regions exist, and a spawn sent to one
    // that is not there is never answered.
    let to: Option<RegionId> = herd_common::arg("to")
        .map(|raw| RegionId::from_raw(raw.parse().unwrap_or_else(|_| {
            eprintln!("--to: cannot read {raw:?}");
            std::process::exit(2);
        })));
    // Entities of this edge's own to walk between the two regions.
    let migrate: usize = herd_common::arg_or("migrate", 0usize);
    if migrate > 0 && to.is_none() {
        eprintln!("--migrate needs --to, which says where they are going");
        std::process::exit(2);
    }

    // This binary owns both ends. Where the broker is, what certificate the
    // edge presents, and which crypto provider is installed are all decided
    // here rather than by the library; see docs/adr/0006.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats = runtime.block_on(herd_common::connect(&url, herd_common::arg("creds"))).unwrap_or_else(|e| {
        eprintln!("nats {url}: {e}");
        std::process::exit(1);
    });
    let quic = edge_endpoint(&listen, runtime.handle());

    let herd_state = Arc::new(Mutex::new(Herd::default()));
    let clients = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let herd_state = Arc::clone(&herd_state);
        let clients = Arc::clone(&clients);
        EdgeServer::new(nats, runtime.handle().clone(), quic, move |handle| Game {
            handle,
            herd: herd_state,
            clients,
        })
    }
    .unwrap_or_else(|e| {
        eprintln!("starting the edge: {e}");
        std::process::exit(1);
    });
    server.set_heartbeat_interval(Duration::from_secs(herd_common::arg_or("heartbeat", 30u64)));

    let handle = server.handle();
    println!("herd-edge: {} listening on {listen}", server.name());

    // A lane of its own, well clear of where a game puts its clients, and
    // inside the region: a position outside it is refused by the region and
    // reported to nobody, which leaves the entity asked for and never answered.
    let lane = 2048 + (std::process::id() % 30) as i32 * 60;
    if let Some(other) = to {
        println!("herd-edge: walking {migrate} of its own between {region} and {other}");
        for k in 0..migrate {
            let at = Pos3::from_meters(lane, 64 + (k as i32 % 3072), 0);
            match handle.spawn_detached(region, at, EntityKind::Unattended) {
                Ok(key) => {
                    herd_state
                        .lock()
                        .expect("not poisoned")
                        .live
                        .insert(key, Traveler { region, at, heading: 1 });
                }
                Err(e) => {
                    eprintln!("asking for a traveler: {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    let stop = AtomicBool::new(false);
    let period = Duration::from_millis(50);
    let step = Fixed::from_raw(Fixed::from_meters(WALK_M_PER_SEC).raw() / 20);
    let mut reported = Instant::now();
    let mut cursor = 0usize;

    while !stop.load(Ordering::Relaxed) {
        let deadline = Instant::now() + period;

        // Walk whatever this edge holds of its own.
        {
            let mut herd = herd_state.lock().expect("not poisoned");
            let moves: Vec<(EntityKey, Pos3)> = herd
                .live
                .iter_mut()
                .map(|(key, t)| {
                    let moved = Fixed::from_raw(t.at.x.raw() + step.raw() * t.heading);
                    if (moved.floor_meters() - lane).abs() > RANGE_M {
                        t.heading = -t.heading;
                    } else {
                        t.at.x = moved;
                    }
                    (*key, t.at)
                })
                .collect();
            drop(herd);
            let _ = handle.move_entities(&moves);
        }

        if reported.elapsed() >= Duration::from_secs(1) {
            // Steps one and two of docs/adr/0003: ask the other region for it,
            // at the position the game chose, and record that its origin copy
            // is waiting on the answer. Step three happens in `spawned`.
            //
            // A cursor sweeping the herd rather than taking the front of it: an
            // arrival goes in at whatever slot the map gives it, and taking a
            // fixed prefix would walk the same few back and forth.
            if let Some(other) = to {
                let herd = herd_state.lock().expect("not poisoned");
                let settled: Vec<(EntityKey, RegionId, Pos3)> = herd
                    .live
                    .iter()
                    .filter(|(key, _)| !herd.in_transit.values().any(|was| was == *key))
                    .map(|(key, t)| (*key, t.region, t.at))
                    .collect();
                if !settled.is_empty() {
                    let take = migrate.min(settled.len()).div_ceil(4).max(1);
                    let mut going = Vec::with_capacity(take);
                    for k in 0..take {
                        going.push(settled[(cursor + k) % settled.len()]);
                    }
                    cursor = (cursor + take) % settled.len();
                    drop(herd);
                    for (was, from, at) in going {
                        // The same coordinates in the other region: both are
                        // 4096 m, so a door is wherever the game says it is.
                        let there = if from == region { other } else { region };
                        if let Ok(key) =
                            handle.spawn_detached(there, at, EntityKind::Unattended)
                        {
                            herd_state
                                .lock()
                                .expect("not poisoned")
                                .in_transit
                                .insert(key, was);
                        }
                    }
                }
            }

            let stats = handle.stats();
            let herd = herd_state.lock().expect("not poisoned");
            let away = herd.live.values().filter(|t| t.region != region).count();
            println!(
                "herd-edge: {} clients | {} entities ({} observing) | \
                 relayed {} undeliverable {} | commands {} refused {} | \
                 herd {} ({away} away, {} in flight, {} migrated)",
                stats.clients,
                stats.entities,
                stats.observers,
                stats.relayed,
                stats.undeliverable,
                stats.commands,
                stats.refused,
                herd.live.len(),
                herd.in_transit.len(),
                herd.migrated,
            );
            reported = Instant::now();
        }

        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
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
