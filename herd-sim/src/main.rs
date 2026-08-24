//! The simulation binary.
//!
//! Owns a [`WorldSimulation`], supplies the game, and drives the tick loop.
//! Nothing here opens a socket: payloads leave a simulation through a sink, and
//! the sink that speaks to `herd-edge` is not built.
//!
//! Step 1 of the build order. A population spawns on the first tick and holds
//! still after it. Movement, viewers and health follow, in that order.

use umwelt::{Fixed, Game, Pos3, Step, TickStats, WorldConfig, WorldSimulation};

/// Entities the region carries. Matches the scale umwelt's own benchmarks
/// report, so figures from here are comparable against them.
const ENTITIES: usize = 50_000;

/// Ticks per run. Twenty seconds of simulated time at 20 Hz.
const TICKS: u32 = 400;

/// Ticks between report lines. One second of simulated time at 20 Hz.
const REPORT_EVERY: u32 = 20;

/// Fixed, so a run reproduces.
const SEED: u64 = 0x4845_5244;

/// Meters held clear of the region edge at spawn, so movement has room before
/// it meets one.
const MARGIN_M: i32 = 8;

/// xorshift64.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u32) -> u32 {
        (self.next_u64() % bound as u64) as u32
    }
}

/// The game.
///
/// Spawns a population on the first tick and leaves it alone after that.
/// Movement is step 2 of the build order and health step 4.
struct Herd {
    pending: Vec<Pos3>,
}

impl Herd {
    fn new(cfg: &WorldConfig, n: usize, seed: u64) -> Herd {
        let mut rng = Rng::new(seed);
        let margin = Fixed::from_meters(MARGIN_M).raw() as u32;
        let extent = cfg.region_size().raw() as u32 - 2 * margin;
        let vertical = cfg.vertical_extent().raw() as u32;
        let pending = (0..n)
            .map(|_| {
                Pos3::new(
                    Fixed::from_raw((margin + rng.below(extent)) as i32),
                    Fixed::from_raw((margin + rng.below(extent)) as i32),
                    Fixed::from_raw(rng.below(vertical) as i32),
                )
            })
            .collect();
        Herd { pending }
    }
}

impl Game for Herd {
    fn step(&mut self, w: &mut Step<'_>) {
        for p in core::mem::take(&mut self.pending) {
            w.spawn(p);
        }
    }
}

/// A run's tick stats, summed. `TickStats::merge` is private to umwelt, so a
/// consumer accumulating across ticks adds the fields it wants. `sink_nanos` is
/// left out: it is wall clock summed across workers, not a count.
#[derive(Default)]
struct Totals {
    viewers: u64,
    candidates: u64,
    records: u64,
    bytes: u64,
}

impl Totals {
    fn add(&mut self, s: &TickStats) {
        self.viewers += s.viewers;
        self.candidates += s.candidates;
        self.records += s.records;
        self.bytes += s.bytes;
    }
}

fn main() {
    let cfg = WorldConfig::default();
    let mut sim = WorldSimulation::new(cfg, Herd::new(&cfg, ENTITIES, SEED));

    println!(
        "herd-sim: {ENTITIES} entities, {} m region, {} Hz, {} workers",
        cfg.region_size().floor_meters(),
        cfg.tick_hz(),
        sim.thread_count()
    );
    println!("No clock: ticks run back to back, so elapsed time is throughput and not a schedule.");
    println!("No viewers are registered, so the replication columns are zero by construction.");
    println!();
    println!("{:>6} {:>9} {:>8} {:>9} {:>8}", "tick", "entities", "viewers", "records", "bytes");

    let started = std::time::Instant::now();
    let mut totals = Totals::default();
    for tick in 1..=TICKS {
        let stats = sim.tick();
        totals.add(&stats);
        if tick % REPORT_EVERY == 0 {
            println!(
                "{:>6} {:>9} {:>8} {:>9} {:>8}",
                tick,
                sim.entity_count(),
                stats.viewers,
                stats.records,
                stats.bytes
            );
        }
    }
    let elapsed = started.elapsed();

    println!();
    println!(
        "{TICKS} ticks in {:.2} s, {:.2} ms per tick against a {:.0} ms budget",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / TICKS as f64,
        1000.0 / cfg.tick_hz() as f64
    );
    println!(
        "totals: {} viewers served, {} candidates, {} records, {} bytes",
        totals.viewers, totals.candidates, totals.records, totals.bytes
    );
}
