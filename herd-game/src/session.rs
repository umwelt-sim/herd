//! One connection to an edge, and the loop that drives it.
//!
//! The sending half of what a game developer writes: connect, ask for a
//! population, then send where everything is. What comes back is
//! [`crate::watcher`]'s. What the entities do is [`crate::crowd`]'s.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{ClientHandle, EdgeClient};

use crate::Options;
use crate::crowd::Crowd;
use crate::watcher::Reports;

/// Plays one client until it is stopped or the edge goes away.
pub fn play(
    runtime: &tokio::runtime::Handle,
    endpoint: &quinn::Endpoint,
    options: &Options,
    n: usize,
    stop: &AtomicBool,
) {
    let (client, reports) = connect(runtime, endpoint, options);
    let sending: ClientHandle = client.handle();

    // A column of its own per client, so several do not stack on one spot.
    let lane = ((std::process::id() as usize + n * 7) % 64) as i32 * 60 + 64;
    let mut crowd = Crowd::new(
        &sending,
        options.region,
        lane,
        options.send_hz,
        options.observers,
        options.unattended,
    )
    .unwrap_or_else(|e| {
        eprintln!("asking for a population: {e}");
        std::process::exit(1);
    });

    let period = Duration::from_millis(1_000 / options.send_hz.max(1) as u64);
    let mut reported = Instant::now();
    let mut sent = 0u64;
    let mut given_back = 0u64;

    while !stop.load(Ordering::Relaxed) && !reports.gone() {
        let deadline = Instant::now() + period;

        crowd.settle(&reports);
        match crowd.walk(&sending) {
            Ok(moved) => sent += moved as u64,
            Err(_) => return,
        }

        if reported.elapsed() >= Duration::from_secs(1) {
            match crowd.churn(&sending, options.churn) {
                Ok(gone) => given_back += gone as u64,
                Err(_) => return,
            }
            let (packets, records) = reports.take_traffic();
            println!(
                "herd-game[{n}]: holding {} ({} with ids) | {sent} moves sent | \
                 {packets} packets | {records} records | {given_back} handed back",
                crowd.len(),
                crowd.confirmed(),
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

/// Opens the connection and hands it to umwelt.
///
/// This binary owns its connection, so where the edge is and what it has to
/// present to be believed are decided here rather than by umwelt. umwelt owns
/// the game from this point and calls it; nothing else ever polls.
fn connect(
    runtime: &tokio::runtime::Handle,
    endpoint: &quinn::Endpoint,
    options: &Options,
) -> (EdgeClient, Reports) {
    let target = options.addr.parse().unwrap_or_else(|e| {
        eprintln!("--edge {:?}: {e}", options.addr);
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
            eprintln!("connecting to {}: {e}", options.addr);
            std::process::exit(1);
        });

    let reports = Reports::default();
    let watcher = reports.watcher();
    let client = EdgeClient::new(conn, runtime.clone(), |_sending| watcher)
        .unwrap_or_else(|e| {
            eprintln!("opening a stream: {e}");
            std::process::exit(1);
        });
    (client, reports)
}
