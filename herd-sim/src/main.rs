//! The simulation binary.
//!
//! Owns a [`WorldSimulation`], supplies the game, and drives the tick loop.
//! Nothing here opens a socket: payloads leave a simulation through a sink, and
//! the sink that speaks to `herd-edge` is not built.
//!
//! Step 2 of the build order. Entities walk to a destination and pick another
//! on arrival.
//!
//! Attractors are places. An entity that spawned at one is a resident and keeps
//! it as a home for the whole run; the rest are nomads and wander. The world
//! alternates between gathering and dispersing, so residents crowd into their
//! home, spread back out around it, and crowd again. Crowd sizes return to the
//! same figures each cycle, since a resident's home never changes, and the tick
//! is exercised at both ends of the range rather than settling at one.
//!
//! umwelt owns the loop: it has the tick rate, so `run` keeps the schedule and
//! this binary supplies a [`Game`] and an observer. Free-running is available
//! for studying the population, where simulated minutes matter and wall clock
//! does not.
//!
//! Viewers and health follow.

use umwelt::{
    CellId, DistSq, Fixed, Flow, Game, Pacing, Pos2, Pos3, Step, TickStats, Wait, WorldConfig,
    WorldSimulation,
};

/// Entities the region carries. Matches the scale umwelt's own benchmarks
/// report, so figures from here are comparable against them.
const ENTITIES: usize = 50_000;

/// Ticks per run unless the command line says otherwise. One minute of wall
/// clock at 20 Hz. A full gather and disperse cycle is six simulated minutes,
/// which is six real minutes unless the clock is off.
const TICKS: u32 = 1_200;

/// Report lines per run, whatever its length.
const REPORTS: u32 = 20;

/// Fixed, so a run reproduces.
const SEED: u64 = 0x4845_5244;

/// Meters held clear of the region edge, so a destination is always inside it.
const MARGIN_M: i32 = 8;

/// Places the population is drawn toward. Eight, matching the count umwelt's
/// clustered benchmark fixture places by hand, so the two are comparable.
const ATTRACTORS: usize = 8;

/// How far from an attractor's center a destination lands. Half a cell at the
/// default 128 m cell size, so a crowd sits inside one or two cells. A guess.
const ATTRACTOR_RADIUS_M: i32 = 64;

/// Retargets by a nomad that head for an attractor rather than wandering, in
/// sixteenths. A guess: high enough that a crowd draws passers-by, low enough
/// that the ground between attractors is not empty.
const ATTRACT_IN_16: u32 = 4;

/// How far a resident spreads from home while the world is dispersing. One view
/// radius, so a resident leaves and re-enters the range of anyone watching its
/// home.
const DISPERSE_RADIUS_M: i32 = 256;

/// Seconds the world spends gathering, then the same dispersing.
///
/// Bounded below by the slowest class that has to cross a dispersal radius
/// inside one phase: a walker at 1.5 m/s covers 256 m in 171 s, so a phase
/// shorter than that leaves walkers permanently in transit and the swing comes
/// only from the fast classes.
const PHASE_S: i32 = 180;

/// Entities that spawn near an attractor rather than anywhere, in sixteenths.
/// Only a class that can move is eligible: an immobile prop placed in a crowd
/// pins that crowd, since it is there for every gather phase and every disperse
/// phase alike. Scenery is spread over the region instead.
/// Ten of sixteen is 62.5%, near the 60% umwelt's clustered fixture places in
/// its eight cells, so the two start comparable. A population is dense around
/// a place before a simulation starts, not because it walked there during
/// warmup: at the slowest class in the mix, walking across the region takes
/// simulated hours.
const SPAWN_AT_ATTRACTOR_IN_16: u32 = 14;

/// Seconds an entity stands at an attractor before moving on, drawn between
/// these. A crowd exists because people stay: without a dwell an attractor is
/// a waypoint, and the density around it is set by who happens to be in
/// transit. Both ends are guesses.
const DWELL_S: (i32, i32) = (5, 60);

/// Seconds of travel an entity commits to at once. Bounds how far it will
/// consider going, so a slow class stays local and a fast one crosses the
/// region. A guess.
const HORIZON_S: i32 = 60;

/// Motion classes as (share in percent, meters per second, and thousandths).
/// Carried over from umwelt's quality harness, where the mix is **chosen rather
/// than measured against any real game**.
const MIX: [(u32, i32, i32); 5] =
    [(35, 0, 0), (25, 0, 200), (25, 1, 500), (10, 6, 0), (5, 30, 0)];

/// Tick durations, in quarter-octave buckets on microseconds, so a percentile
/// is not pinned to a power of two.
struct Histogram {
    buckets: [u64; 128],
    count: u64,
    max_us: u64,
}

impl Histogram {
    fn new() -> Histogram {
        Histogram { buckets: [0; 128], count: 0, max_us: 0 }
    }

    fn record(&mut self, us: u64) {
        let e = us.max(1);
        let oct = e.ilog2();
        let b = if oct < 2 {
            oct as usize * 4
        } else {
            ((oct * 4 + ((e >> (oct - 2)) & 3) as u32) as usize).min(127)
        };
        self.buckets[b] += 1;
        self.count += 1;
        self.max_us = self.max_us.max(us);
    }

    /// Upper edge of the bucket holding the `q` quantile, in microseconds.
    /// Log-spaced buckets, so this is a bound rather than an interpolation.
    fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count as f64 * q) as u64;
        let mut acc = 0;
        for (b, n) in self.buckets.iter().enumerate() {
            acc += n;
            if acc >= target {
                let (oct, sub) = (b as u32 / 4, b as u32 % 4);
                return if oct < 2 { 1 << oct } else { ((4 + sub) as u64) << (oct - 2) };
            }
        }
        self.max_us
    }
}

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

    /// Signed, in `-bound..=bound`.
    fn spread(&mut self, bound: i32) -> i32 {
        self.below(2 * bound as u32 + 1) as i32 - bound
    }
}

/// The game.
///
/// Per-entity state is parallel to umwelt's position arrays and indexed by
/// entity id, which spawn assigns in order and never reuses.
struct Herd {
    pending: Vec<Pos3>,
    attractors: Vec<Pos2>,
    /// Raw units per tick along each axis. Zero for a class that never moves.
    vel_x: Vec<i32>,
    vel_y: Vec<i32>,
    /// Raw units per tick, the magnitude the velocity was built to.
    speed: Vec<i32>,
    /// Where the entity is headed, raw.
    dest: Vec<Pos2>,
    /// Raw units this entity will travel toward one destination.
    horizon: Vec<i32>,
    /// Ticks left standing where it is.
    wait: Vec<u32>,
    /// Ticks to stand for on reaching the current destination. Zero for a
    /// destination not worth standing at.
    dwell: Vec<u32>,
    /// The attractor a resident belongs to. `None` is a nomad, which has no
    /// home to gather at and wanders instead.
    home: Vec<Option<u8>>,
    /// The phase the current destination was chosen under, so a change of
    /// phase can be noticed by an entity that is still walking.
    phase_at: Vec<bool>,
    rng: Rng,
    lo: i32,
    hi: i32,
    tick_hz: i32,
}

impl Herd {
    fn new(cfg: &WorldConfig, n: usize, seed: u64) -> Herd {
        let mut rng = Rng::new(seed);
        let lo = Fixed::from_meters(MARGIN_M).raw();
        let hi = cfg.region_size().raw() - lo;
        let vertical = cfg.vertical_extent().raw() as u32;
        let hz = cfg.tick_hz() as i32;

        let attractors = (0..ATTRACTORS)
            .map(|_| Pos2::new(Fixed::from_raw(rng_in(&mut rng, lo, hi)), Fixed::from_raw(rng_in(&mut rng, lo, hi))))
            .collect();

        // Cumulative shares, so one draw picks a class.
        let mut cuts = [0u32; MIX.len()];
        let mut acc = 0;
        for (k, (share, _, _)) in MIX.iter().enumerate() {
            acc += share;
            cuts[k] = acc;
        }

        let mut herd = Herd {
            pending: Vec::with_capacity(n),
            attractors,
            vel_x: vec![0; n],
            vel_y: vec![0; n],
            speed: vec![0; n],
            dest: vec![Pos2::new(Fixed::ZERO, Fixed::ZERO); n],
            horizon: vec![0; n],
            wait: vec![0; n],
            dwell: vec![0; n],
            home: vec![None; n],
            phase_at: vec![true; n],
            rng,
            lo,
            hi,
            tick_hz: hz,
        };

        for i in 0..n {
            let roll = herd.rng.below(100);
            let class = cuts.iter().position(|&c| roll < c).unwrap_or(MIX.len() - 1);
            let (_, m, milli) = MIX[class];
            herd.speed[i] = Fixed::from_millis(m, milli).raw() / hz;
            herd.horizon[i] = herd.speed[i].saturating_mul(HORIZON_S * hz);

            let resident =
                herd.speed[i] > 0 && herd.rng.below(16) < SPAWN_AT_ATTRACTOR_IN_16;
            let at = if resident {
                let h = herd.rng.below(ATTRACTORS as u32) as u8;
                herd.home[i] = Some(h);
                let a = herd.attractors[h as usize];
                herd.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
            } else {
                Pos2::new(
                    Fixed::from_raw(rng_in(&mut herd.rng, lo, hi)),
                    Fixed::from_raw(rng_in(&mut herd.rng, lo, hi)),
                )
            };
            herd.pending.push(at.at_height(Fixed::from_raw(herd.rng.below(vertical) as i32)));
            herd.retarget(i, at, true);
        }
        herd
    }

    /// Picks a destination and the velocity that walks to it. One divide, paid
    /// on arrival rather than per tick.
    fn retarget(&mut self, i: usize, at: Pos2, gathering: bool) {
        self.phase_at[i] = gathering;
        let speed = self.speed[i];
        if speed == 0 {
            return;
        }
        let hz = self.tick_hz;
        let dest = match self.home[i] {
            // A resident goes home to gather and spreads around home to
            // disperse, so its crowd returns to the same size every cycle.
            Some(h) => {
                let a = self.attractors[h as usize];
                if gathering {
                    let (lo, hi) = DWELL_S;
                    self.dwell[i] = (lo * hz + self.rng.below(((hi - lo) * hz) as u32) as i32) as u32;
                    self.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
                } else {
                    self.dwell[i] = 0;
                    self.offset(a, Fixed::from_meters(DISPERSE_RADIUS_M).raw())
                }
            }
            // A nomad has nowhere to be. It mostly wanders, and occasionally
            // visits whichever attractor it can reach.
            None if self.rng.below(16) < ATTRACT_IN_16 => {
                let reach = DistSq::from_radius(Fixed::from_raw(self.horizon[i]));
                let near: Vec<Pos2> =
                    self.attractors.iter().copied().filter(|a| at.dist_sq(*a) <= reach).collect();
                let a = if near.is_empty() {
                    *self
                        .attractors
                        .iter()
                        .min_by_key(|a| at.dist_sq(**a))
                        .expect("attractors exist")
                } else {
                    near[self.rng.below(near.len() as u32) as usize]
                };
                let (lo, hi) = DWELL_S;
                self.dwell[i] = (lo * hz + self.rng.below(((hi - lo) * hz) as u32) as i32) as u32;
                self.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
            }
            None => {
                self.dwell[i] = 0;
                self.offset(at, self.horizon[i])
            }
        };

        let dx = (dest.x.raw() - at.x.raw()) as i64;
        let dy = (dest.y.raw() - at.y.raw()) as i64;
        let dist = at.dist_sq(dest).sqrt_approx().raw() as i64;
        self.dest[i] = dest;
        if dist == 0 {
            self.vel_x[i] = 0;
            self.vel_y[i] = 0;
            return;
        }
        self.vel_x[i] = (dx * speed as i64 / dist) as i32;
        self.vel_y[i] = (dy * speed as i64 / dist) as i32;
    }

    /// A point within `radius` of `anchor`, held inside the region.
    fn offset(&mut self, anchor: Pos2, radius: i32) -> Pos2 {
        let x = (anchor.x.raw() + self.rng.spread(radius)).clamp(self.lo, self.hi);
        let y = (anchor.y.raw() + self.rng.spread(radius)).clamp(self.lo, self.hi);
        Pos2::new(Fixed::from_raw(x), Fixed::from_raw(y))
    }
}

/// Whether the world is drawing residents toward home or pushing them out.
/// Phases alternate and are equal in length.
fn gathering(tick: u32, phase_ticks: u32) -> bool {
    (tick / phase_ticks) % 2 == 0
}

/// A coordinate in `lo..=hi`, raw.
fn rng_in(rng: &mut Rng, lo: i32, hi: i32) -> i32 {
    lo + rng.below((hi - lo) as u32) as i32
}

impl Game for Herd {
    fn step(&mut self, w: &mut Step<'_>) {
        let gathering = gathering(w.tick(), (PHASE_S * self.tick_hz) as u32);
        if !self.pending.is_empty() {
            for (i, p) in core::mem::take(&mut self.pending).into_iter().enumerate() {
                let id = w.spawn(p);
                assert_eq!(id.index(), i, "spawn assigns ids in order from zero");
            }
            return;
        }

        let (xs, ys, _) = w.positions_mut();
        for i in 0..xs.len() {
            if self.speed[i] == 0 {
                continue;
            }
            let at = Pos2::new(xs[i], ys[i]);
            if self.phase_at[i] != gathering {
                // A phase reaches everyone at once. Left to notice at the end
                // of an errand, a class slow enough that an errand outlasts the
                // phase never responds to one at all: an idler at 0.2 m/s
                // walking 256 m is in transit for seven of them.
                self.wait[i] = 0;
                self.retarget(i, at, gathering);
            } else if self.wait[i] > 0 {
                self.wait[i] -= 1;
                continue;
            } else if at.dist_sq(self.dest[i])
                <= DistSq::from_radius(Fixed::from_raw(self.speed[i]))
            {
                // Arrival is within one tick's travel, since a step never lands
                // exactly on a destination. Stand first if this destination was
                // worth standing at; the next tick arrives again, finds no
                // dwell left, and moves on.
                self.wait[i] = core::mem::take(&mut self.dwell[i]);
                if self.wait[i] == 0 {
                    self.retarget(i, at, gathering);
                }
                continue;
            }
            xs[i] = Fixed::from_raw(xs[i].raw() + self.vel_x[i]);
            ys[i] = Fixed::from_raw(ys[i].raw() + self.vel_y[i]);
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

/// How the population sits in cells, read through the snapshot a consumer can
/// see. The checkpoint for movement: whether crowds form on their own.
struct Occupancy {
    occupied: usize,
    cells: usize,
    max: usize,
    top_share: f64,
}

fn occupancy(sim: &WorldSimulation<Herd>) -> Occupancy {
    let snap = sim.snapshot();
    let cells = snap.cell_count();
    let mut counts: Vec<usize> = (0..cells)
        .map(|i| snap.entities_for_cell(CellId::from_raw(i as u32)).len())
        .collect();
    let total: usize = counts.iter().sum();
    let occupied = counts.iter().filter(|&&c| c > 0).count();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let top: usize = counts.iter().take(ATTRACTORS).sum();
    Occupancy {
        occupied,
        cells,
        max: counts.first().copied().unwrap_or(0),
        top_share: if total == 0 { 0.0 } else { top as f64 / total as f64 },
    }
}

fn main() {
    let mut ticks = TICKS;
    let mut wait = Wait::Sleep;
    let mut threads = None;
    let mut entities = ENTITIES;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--free-run" => wait = Wait::None,
            "--hold" => wait = Wait::Hold,
            "--threads" => threads = args.next().and_then(|n| n.parse().ok()),
            "--entities" => entities = args.next().and_then(|n| n.parse().ok()).unwrap_or(entities),
            other => match other.parse() {
                Ok(n) => ticks = n,
                Err(_) => {
                    eprintln!(
                        "usage: herd-sim [ticks] [--free-run|--hold] [--threads N] [--entities N]"
                    );
                    std::process::exit(2)
                }
            },
        }
    }
    let report_every = (ticks / REPORTS).max(1);

    let cfg = WorldConfig::default();
    let mut sim = WorldSimulation::new(cfg, Herd::new(&cfg, entities, SEED));
    if let Some(n) = threads {
        sim.set_thread_count(n);
    }

    println!(
        "herd-sim: {entities} entities, {} m region, {} m cells, {} Hz, {} workers",
        cfg.region_size().floor_meters(),
        cfg.cell_size().floor_meters(),
        cfg.tick_hz(),
        sim.thread_count()
    );
    print!("mix (chosen, not measured):");
    for (share, m, milli) in MIX {
        print!(" {share}% at {m}.{milli:03} m/s,");
    }
    println!();
    match wait {
        Wait::None => println!(
            "Free-running: ticks go back to back, so elapsed time is throughput and not a schedule."
        ),
        w => println!(
            "Clocked at {} Hz by umwelt, {}: {} ticks is {:.1} minutes of wall clock.",
            cfg.tick_hz(),
            if w == Wait::Hold { "holding the core" } else { "sleeping to the deadline" },
            ticks,
            ticks as f64 / cfg.tick_hz() as f64 / 60.0
        ),
    }
    println!("No viewers are registered, so the replication columns are zero by construction.");
    println!();
    println!(
        "{:>6} {:>10} {:>9} {:>8} {:>10} {:>8}",
        "tick", "phase", "occupied", "max cell", "top 8", "records"
    );

    let period = std::time::Duration::from_nanos(1_000_000_000 / cfg.tick_hz() as u64);
    let mut totals = Totals::default();
    let mut work = Histogram::new();
    let mut late = Histogram::new();
    let mut over_budget = 0u64;

    // umwelt owns the loop: it has the tick rate, so it keeps the schedule.
    let summary = sim.run(Pacing { wait, ticks: Some(ticks), ..Pacing::default() }, |r, sim| {
        work.record(r.took.as_micros() as u64);
        if !r.late.is_zero() {
            late.record(r.late.as_micros() as u64);
        }
        if r.took > period {
            over_budget += 1;
        }
        totals.add(&r.stats);

        if r.tick % report_every == 0 {
            let o = occupancy(sim);
            let phase = if gathering(r.tick, (PHASE_S * cfg.tick_hz() as i32) as u32) {
                "gathering"
            } else {
                "dispersing"
            };
            println!(
                "{:>6} {:>10} {:>4}/{:<4} {:>8} {:>9.1}% {:>8}",
                r.tick,
                phase,
                o.occupied,
                o.cells,
                o.max,
                100.0 * o.top_share,
                r.stats.records
            );
        }
        Flow::Continue
    });

    let budget_ms = period.as_secs_f64() * 1000.0;
    println!();
    println!(
        "{} ticks in {:.2} s, {:.2} ms per tick of wall clock against a {budget_ms:.0} ms budget",
        summary.ticks,
        summary.elapsed.as_secs_f64(),
        summary.elapsed.as_secs_f64() * 1000.0 / summary.ticks.max(1) as f64
    );
    println!(
        "tick work: p50 {:.2} ms, p99 {:.2} ms, worst {:.2} ms, {} of {} over budget",
        work.quantile_us(0.50) as f64 / 1000.0,
        work.quantile_us(0.99) as f64 / 1000.0,
        summary.worst_tick.as_secs_f64() * 1000.0,
        over_budget,
        summary.ticks
    );
    if wait != Wait::None {
        println!(
            "schedule: {} of {} ticks started late, p99 lateness {:.2} ms, worst {:.2} ms, {} deadlines dropped",
            summary.late,
            summary.ticks,
            late.quantile_us(0.99) as f64 / 1000.0,
            summary.worst_late.as_secs_f64() * 1000.0,
            summary.dropped
        );
        println!(
            "duty: {:.1}% of the budget spent working at p50, the rest {}",
            100.0 * work.quantile_us(0.50) as f64 / 1000.0 / budget_ms,
            if wait == Wait::Hold { "held" } else { "asleep" }
        );
    }
    println!(
        "totals: {} viewers served, {} candidates, {} records, {} bytes",
        totals.viewers, totals.candidates, totals.records, totals.bytes
    );
}
