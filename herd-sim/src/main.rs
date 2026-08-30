//! The region binary, and the first of herd's three peers.
//!
//! Owns a [`WorldSimulation`], supplies the game, and drives the tick loop. It
//! also serves a region: `herd-edge` reaches it over NATS, and everything an
//! edge spawns lives alongside the population this game creates for itself.
//! Payloads leave through an [`EdgeSink`], wrapped in a [`Handoff`] so the tick
//! never waits on a broker.
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
//! Step 3 registers viewers, which is where the cost of a tick actually lives:
//! everything before this moves entities and rebuilds a snapshot, and none of
//! it depends on anyone watching.
//!
//! Step 7 of umwelt's build order is a bot harness, which is this binary with
//! its movement replaced. A pattern is hostile on purpose and is meant to break
//! something: a pattern that misses a deadline is a result, not a failed run.
//!
//! Step 4 adds health. A crowded cell hurts what stands in it, so a crowd thins
//! from wherever it is densest, and a spawner refills the region. Health never
//! reaches a client, since a record carries a position and nothing else and the
//! opaque payload a consumer would put its own fields in is not built. What
//! reaches a client is the despawn.

use std::sync::Arc;
use std::time::Duration;

use umwelt::net::{EdgeSink, Edges, Inbound, RegionServer};
use umwelt::{
    DistSq, EntityId, Fixed, Flow, Game, Handoff, Pacing, PayloadSink, Pos2, Pos3,
    RegionId, Step, TickStats, Wait, WorldConfig, WorldSimulation,
};

/// Clients watching the region, unless the command line says otherwise. The
/// count umwelt's benchmarks treat as the load a region has to carry.
const VIEWERS: usize = 8_192;

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

/// Ticks per second. All three peers build the same world from
/// `herd_common::world`, so there is one spelling of it and they cannot drift.
const TICK_HZ: u32 = 20;

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

/// Health an entity spawns with, and the most it recovers to.
const HEALTH_MAX: i16 = 30_000;

/// Entities in a cell before it starts hurting them.
///
/// Measured over five simulated minutes: 400 thins the fullest cell to 799 and
/// kills 50.7 a second, 2,500 kills nobody, and 1,500 kills 6.1 a second while
/// leaving the fullest cell at 1,592 against the 1,852 that movement alone
/// settles at. Deaths exercise the despawn path; thinning the crowds removes
/// the load this binary exists to produce.
const CROWD_THRESHOLD: usize = 1_500;

/// Crowding damage per tick is the excess over the threshold divided by this.
/// Sized so that a cell of three thousand kills in about a minute of standing
/// in it, which is longer than the mean dwell: an entity that keeps moving
/// survives, and one that keeps choosing the densest place does not. A guess.
const DAMAGE_DIVISOR: usize = 100;

/// Health recovered per tick anywhere below the threshold.
const REGEN: i16 = 5;

/// How the population moves. `Herd` is a plausible world; the rest exist to
/// break something in particular.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pattern {
    /// Attractors, dwell, and a gather and disperse cycle.
    Herd,
    /// Everything converges on one cell and stays. Against the walk cap and the
    /// gather, while a crowd is forming rather than once it has formed.
    Flash,
    /// Every entity astride a cell boundary, stepping across it each tick.
    /// Against subscription churn, which a plausible world leaves at 0.23% of
    /// viewers a tick.
    Flap,
    /// Entities oscillating in and out of the range a viewer holds ghosts for.
    /// Against `grace` and the departure queue, which is where the one real bug
    /// the pipeline benchmark caught lived.
    Thrash,
    /// A share of the population jumping the region every tick. Against the
    /// accumulator: a jump is the most drift an entity can have, so every one
    /// of them outranks everything else for a packet slot.
    Teleport,
    /// A quarter of the population dying at once, periodically. Against the
    /// despawn queue and the half-packet cap on despawn records.
    Cull,
}

impl Pattern {
    fn parse(s: &str) -> Option<Pattern> {
        Some(match s {
            "herd" => Pattern::Herd,
            "flash" => Pattern::Flash,
            "flap" => Pattern::Flap,
            "thrash" => Pattern::Thrash,
            "teleport" => Pattern::Teleport,
            "cull" => Pattern::Cull,
            _ => return None,
        })
    }
}

/// Meters either side of a cell boundary a [`Pattern::Flap`] entity steps.
const FLAP_M: i32 = 2;

/// Meters per second a [`Pattern::Flash`] entity moves. A crowd that takes
/// twenty minutes to gather is not a flash crowd, and at the plausible mix's
/// walking pace crossing the region takes that long.
const FLASH_SPEED: i32 = 60;

/// Meters per second a [`Pattern::Thrash`] entity moves, chosen so that a
/// hundred meter swing takes a few seconds rather than a minute.
const THRASH_SPEED: i32 = 20;

/// How far out a [`Pattern::Thrash`] entity swings from its attractor.
const THRASH_FAR_M: i32 = 100;

/// Entities per thousand that jump each tick under [`Pattern::Teleport`].
const TELEPORT_PER_MILLE: usize = 1;

/// Seconds between [`Pattern::Cull`] events, and the share that dies in one.
const CULL_PERIOD_S: u32 = 30;
const CULL_IN_16: u32 = 4;

/// Mean seconds an entity lives before it dies of nothing in particular.
///
/// Crowding damage stops once a crowd thins to the threshold, so on its own it
/// gives a burst of deaths and then none. This gives churn that does not depend
/// on density: the region loses `population / lifespan` per tick, which at
/// 50,000 entities and forty minutes is about 21 a second. A guess, and the
/// dial for how much despawn traffic a run carries.
const LIFESPAN_S: u32 = 2_400;

/// The most the spawner replaces in one tick, so a die-off refills over several
/// ticks instead of arriving as one spike.
const MAX_SPAWNS_PER_TICK: usize = 64;

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
        // Rounded up and never zero: truncating puts the target at 0 for a
        // small count, and the first empty bucket then satisfies `acc >=
        // target` and reports a microsecond for a tick that took a millisecond.
        let target = ((self.count as f64 * q).ceil() as u64).max(1);
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

/// xorshift64, hand-rolled rather than taken from `rand`.
///
/// A run reproduces from [`SEED`], and `rand` does not promise value stability
/// across releases: an upgrade would silently change every figure this binary
/// has ever reported. Ten lines that will never change are worth more here than
/// a better generator, and nothing in a load generator needs one.
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
    /// What the edges asked for. Applied inside the tick, which is the only
    /// place an entity can be spawned, moved or despawned.
    inbound: Arc<Inbound>,
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
    /// Whether this game created the entity in this slot.
    ///
    /// An edge spawns into the same region, so slots arrive that this game
    /// never chose a speed or a destination for. Those are left where their
    /// edge puts them. They are still crowded and still hurt by it, because
    /// standing in a crush is the region's business rather than whose entity
    /// it is.
    mine: Vec<bool>,
    /// Entities that both move and belong somewhere. Clients are drawn from
    /// these, so a viewer takes part in the cycle instead of watching it.
    residents: Vec<u32>,
    health: Vec<i16>,
    /// The tick each entity dies on, whatever else happens to it.
    dies_at: Vec<u32>,
    /// Population by cell, rebuilt every tick. umwelt keeps a cell-ordered
    /// snapshot of exactly this, but a `Step` hands out positions and liveness
    /// and no way to ask where anything is, so a consumer that wants to know
    /// counts for itself.
    occupancy: Vec<u32>,
    /// Scratch, so a tick that kills nothing allocates nothing.
    dead: Vec<EntityId>,
    /// What the spawner refills toward.
    target: usize,
    pattern: Pattern,
    /// Mean ticks lived.
    ///
    /// A population that starts together is given ages spread over one of
    /// these, so the region loses `population / lifespan` per tick from the
    /// first tick rather than after a wait. Replacements draw uniformly over
    /// half to one and a half, which keeps the mean and stops a burst of
    /// deaths from echoing as a second burst one lifespan later.
    lifespan: u32,
    /// Crowding damage per tick is the excess over this divided by
    /// `damage_divisor`. It sets the crowd size deaths settle at, which is what
    /// a load generator is really choosing: a threshold well under the crowd
    /// thins it to the threshold, and one above it kills nobody.
    crowd_threshold: usize,
    damage_divisor: usize,
    /// Cumulative counts, reported rather than guessed at.
    pub deaths: u64,
    pub births: u64,
    rng: Rng,
    lo: i32,
    hi: i32,
    tick_hz: i32,
    cfg: WorldConfig,
}

impl Herd {
    fn new(
        cfg: &WorldConfig,
        n: usize,
        seed: u64,
        lifespan: u32,
        pattern: Pattern,
        inbound: Arc<Inbound>,
    ) -> Herd {
        let mut rng = Rng::new(seed);
        let lo = Fixed::from_meters(MARGIN_M).raw();
        let hi = cfg.region_size().raw() - lo;
        let vertical = cfg.vertical_extent().raw() as u32;
        let hz = cfg.tick_hz() as i32;

        let attractors = (0..ATTRACTORS)
            .map(|_| {
                Pos2::new(
                    Fixed::from_raw(rng_in(&mut rng, lo, hi)),
                    Fixed::from_raw(rng_in(&mut rng, lo, hi)),
                )
            })
            .collect();

        // Cumulative shares, so one draw picks a class.
        let mut cuts = [0u32; MIX.len()];
        let mut acc = 0;
        for (k, (share, _, _)) in MIX.iter().enumerate() {
            acc += share;
            cuts[k] = acc;
        }

        let mut herd = Herd {
            inbound,
            pending: Vec::with_capacity(n),
            attractors,
            vel_x: vec![0; n],
            vel_y: vec![0; n],
            speed: vec![0; n],
            dest: vec![Pos2::new(Fixed::ZERO, Fixed::ZERO); n],
            horizon: vec![0; n],
            wait: vec![0; n],
            mine: vec![true; n],
            dwell: vec![0; n],
            home: vec![None; n],
            phase_at: vec![true; n],
            residents: Vec::new(),
            health: vec![HEALTH_MAX; n],
            dies_at: Vec::with_capacity(n),
            occupancy: vec![0; cfg.cells_per_region() as usize],
            dead: Vec::new(),
            target: n,
            pattern,
            lifespan,
            crowd_threshold: CROWD_THRESHOLD,
            damage_divisor: DAMAGE_DIVISOR,
            deaths: 0,
            births: 0,
            rng,
            lo,
            hi,
            tick_hz: hz,
            cfg: *cfg,
        };

        for i in 0..n {
            let roll = herd.rng.below(100);
            let class = cuts.iter().position(|&c| roll < c).unwrap_or(MIX.len() - 1);
            let (_, m, milli) = MIX[class];
            herd.speed[i] = match pattern {
                Pattern::Flap if m > 0 || milli > 0 => Fixed::from_meters(FLAP_M).raw(),
                Pattern::Flash if m > 0 || milli > 0 => {
                    Fixed::from_meters(FLASH_SPEED).raw() / hz
                }
                Pattern::Thrash if m > 0 || milli > 0 => {
                    Fixed::from_meters(THRASH_SPEED).raw() / hz
                }
                _ => Fixed::from_millimeters(m, milli).raw() / hz,
            };
            herd.horizon[i] = herd.speed[i].saturating_mul(HORIZON_S * hz);

            // Every pattern but the plausible one needs viewers to draw from,
            // and draws them from whatever moves.
            let resident = herd.speed[i] > 0
                && (pattern != Pattern::Herd
                    || herd.rng.below(16) < SPAWN_AT_ATTRACTOR_IN_16);
            let at = if resident {
                let h = herd.rng.below(ATTRACTORS as u32) as u8;
                herd.home[i] = Some(h);
                herd.residents.push(i as u32);
                let a = herd.attractors[h as usize];
                herd.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
            } else {
                Pos2::new(
                    Fixed::from_raw(rng_in(&mut herd.rng, lo, hi)),
                    Fixed::from_raw(rng_in(&mut herd.rng, lo, hi)),
                )
            };
            // Flapping needs somewhere to flap across, so it starts a meter
            // inside a cell with the boundary a step away.
            let at = if pattern == Pattern::Flap && herd.speed[i] > 0 {
                let cell = cfg.cell_size().raw();
                let edge = (at.x.raw() / cell + 1) * cell;
                let inside = Fixed::from_meters(1).raw();
                Pos2::new(Fixed::from_raw((edge - inside).clamp(lo, hi)), at.y)
            } else {
                at
            };
            herd.pending
                .push(at.at_height(Fixed::from_raw(herd.rng.below(vertical) as i32)));
            let dies_at = 1 + herd.rng.below(herd.lifespan);
            herd.dies_at.push(dies_at);
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
        if let Some(dest) = self.pattern_dest(i, at) {
            self.aim(i, at, dest);
            return;
        }
        let dest = match self.home[i] {
            // A resident goes home to gather and spreads around home to
            // disperse, so its crowd returns to the same size every cycle.
            Some(h) => {
                let a = self.attractors[h as usize];
                if gathering {
                    let (lo, hi) = DWELL_S;
                    self.dwell[i] =
                        (lo * hz + self.rng.below(((hi - lo) * hz) as u32) as i32) as u32;
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
                let near: Vec<Pos2> = self
                    .attractors
                    .iter()
                    .copied()
                    .filter(|a| at.dist_sq(*a) <= reach)
                    .collect();
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
                self.dwell[i] =
                    (lo * hz + self.rng.below(((hi - lo) * hz) as u32) as i32) as u32;
                self.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
            }
            None => {
                self.dwell[i] = 0;
                self.offset(at, self.horizon[i])
            }
        };

        self.aim(i, at, dest);
    }

    /// Where a hostile pattern sends an entity, or `None` for the plausible
    /// one, which has its own rules.
    fn pattern_dest(&mut self, i: usize, at: Pos2) -> Option<Pos2> {
        let radius = Fixed::from_meters(ATTRACTOR_RADIUS_M).raw();
        match self.pattern {
            Pattern::Herd | Pattern::Teleport | Pattern::Cull | Pattern::Flap => None,
            // One cell, everyone, no dwell.
            Pattern::Flash => {
                self.dwell[i] = 0;
                Some(self.offset(self.attractors[0], radius))
            }
            // In to the attractor, then out past where a viewer standing in it
            // still holds a ghost, then back.
            Pattern::Thrash => {
                let a = self.attractors[self.home[i].unwrap_or(0) as usize];
                let far = Fixed::from_meters(THRASH_FAR_M).raw();
                let out = at.dist_sq(a) > DistSq::from_radius(Fixed::from_raw(far / 2));
                self.dwell[i] = 0;
                Some(self.offset(a, if out { radius } else { far }))
            }
        }
    }

    /// Points an entity at a destination and builds the velocity that walks it
    /// there. One divide, paid on arrival rather than per tick.
    fn aim(&mut self, i: usize, at: Pos2, dest: Pos2) {
        let speed = self.speed[i];
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

impl Herd {
    /// Entities a client could plausibly be attached to.
    fn residents(&self) -> &[u32] {
        &self.residents
    }

    /// Slots ever allocated. umwelt never reuses one, and reports how many are
    /// live rather than how many exist, so the game's own arrays are the
    /// measure of what a long run has grown to.
    fn slots(&self) -> usize {
        self.health.len()
    }

    /// Replaces one entity. Slots are never reused, so this appends to every
    /// array the game keeps beside umwelt's own, and a long run grows all of
    /// them for as long as anything dies.
    fn spawn_one(&mut self, w: &mut Step<'_>, gathering: bool) {
        let i = w.id_space();
        let (m, milli) = {
            let roll = self.rng.below(100);
            let mut acc = 0;
            let mut class = MIX.len() - 1;
            for (k, (share, _, _)) in MIX.iter().enumerate() {
                acc += share;
                if roll < acc {
                    class = k;
                    break;
                }
            }
            (MIX[class].1, MIX[class].2)
        };
        let speed = Fixed::from_millimeters(m, milli).raw() / self.tick_hz;

        let resident = speed > 0 && self.rng.below(16) < SPAWN_AT_ATTRACTOR_IN_16;
        let home = resident.then(|| self.rng.below(ATTRACTORS as u32) as u8);
        let at = match home {
            Some(h) => {
                let a = self.attractors[h as usize];
                self.offset(a, Fixed::from_meters(ATTRACTOR_RADIUS_M).raw())
            }
            None => Pos2::new(
                Fixed::from_raw(rng_in(&mut self.rng, self.lo, self.hi)),
                Fixed::from_raw(rng_in(&mut self.rng, self.lo, self.hi)),
            ),
        };
        let vertical = self.cfg.vertical_extent().raw() as u32;
        let z = Fixed::from_raw(self.rng.below(vertical) as i32);

        self.adopt(i);
        self.mine.push(true);
        self.vel_x.push(0);
        self.vel_y.push(0);
        self.speed.push(speed);
        self.dest.push(at);
        self.horizon.push(speed.saturating_mul(HORIZON_S * self.tick_hz));
        self.wait.push(0);
        self.dwell.push(0);
        self.home.push(home);
        self.phase_at.push(gathering);
        self.health.push(HEALTH_MAX);
        let dies_at =
            w.tick().wrapping_add(self.lifespan / 2 + 1 + self.rng.below(self.lifespan));
        self.dies_at.push(dies_at);

        let id = w.spawn(at.at_height(z));
        assert_eq!(id.index(), i, "spawn appends, so the game's arrays stay parallel");
        if home.is_some() {
            self.residents.push(i as u32);
        }
        self.retarget(i, at, gathering);
    }
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

        // Everything the edges asked for, applied before this game's own
        // movement so an entity spawned this tick is walked from where it was
        // put. Ordered after the block above so the first tick's population
        // still gets ids from zero.
        self.inbound.apply(w);
        // Those spawns appended slots this game has no state for. Give them
        // inert state so every array stays parallel to umwelt's.
        self.adopt(w.id_space());

        // Slots are never reused, so this walks every entity that ever lived
        // and tests each. `positions_mut` hands liveness back with the slices,
        // so the test is available without recording it first.
        let tick = w.tick();
        let cfg = self.cfg;
        let cells = cfg.cells_per_axis() as usize;
        self.occupancy.fill(0);

        let (xs, ys, _, live) = w.positions_mut();
        for i in 0..xs.len() {
            if !live.contains(EntityId::from_raw(i as u32)) {
                continue;
            }
            if !self.mine[i] || self.speed[i] == 0 {
                continue;
            }
            // Flapping does not walk anywhere: it steps back and forth across
            // the boundary it was placed on, one crossing a tick.
            if self.pattern == Pattern::Flap {
                let step = if self.phase_at[i] { self.speed[i] } else { -self.speed[i] };
                self.phase_at[i] = !self.phase_at[i];
                xs[i] = Fixed::from_raw(xs[i].raw() + step);
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

        match self.pattern {
            Pattern::Teleport => {
                let jumps = xs.len() * TELEPORT_PER_MILLE / 1_000;
                for _ in 0..jumps {
                    let i = self.rng.below(xs.len() as u32) as usize;
                    if !live.contains(EntityId::from_raw(i as u32))
                        || !self.mine[i]
                        || self.speed[i] == 0
                    {
                        continue;
                    }
                    xs[i] = Fixed::from_raw(rng_in(&mut self.rng, self.lo, self.hi));
                    ys[i] = Fixed::from_raw(rng_in(&mut self.rng, self.lo, self.hi));
                }
            }
            Pattern::Cull if tick % (CULL_PERIOD_S * self.tick_hz as u32) == 0 => {
                for i in 0..xs.len() {
                    if live.contains(EntityId::from_raw(i as u32))
                        && self.mine[i]
                        && self.rng.below(16) < CULL_IN_16
                    {
                        // Past anything regeneration can pull back: the health
                        // pass runs after this one, and zero would survive it.
                        self.health[i] = i16::MIN;
                    }
                }
            }
            _ => {}
        }

        // Where everything ended up, so crowding is measured after the move.
        for i in 0..xs.len() {
            if live.contains(EntityId::from_raw(i as u32)) {
                self.occupancy[cell_index(&cfg, xs[i], ys[i], cells)] += 1;
            }
        }

        self.dead.clear();
        for i in 0..xs.len() {
            let id = EntityId::from_raw(i as u32);
            if !live.contains(id) {
                continue;
            }
            let crowd = self.occupancy[cell_index(&cfg, xs[i], ys[i], cells)] as usize;
            self.health[i] = if crowd > self.crowd_threshold {
                let hurt = ((crowd - self.crowd_threshold) / self.damage_divisor) as i16;
                self.health[i].saturating_sub(hurt)
            } else {
                self.health[i].saturating_add(REGEN).min(HEALTH_MAX)
            };
            if self.health[i] <= 0 || tick >= self.dies_at[i] {
                self.dead.push(id);
            }
        }

        for id in core::mem::take(&mut self.dead) {
            w.despawn(id);
            self.deaths += 1;
        }

        // Refill toward the population the region was built with, a few at a
        // time so a die-off does not arrive back as one spike.
        let missing = self.target.saturating_sub(w.entity_count());
        for _ in 0..missing.min(MAX_SPAWNS_PER_TICK) {
            self.spawn_one(w, gathering);
            self.births += 1;
        }
    }
}

impl Herd {
    /// Grows every per-entity array to cover `slots`.
    ///
    /// Slots this game did not create get state that leaves them alone: no
    /// speed, no home, and no lifespan, so they die of crowding or not at all.
    fn adopt(&mut self, slots: usize) {
        if self.mine.len() >= slots {
            return;
        }
        self.mine.resize(slots, false);
        self.vel_x.resize(slots, 0);
        self.vel_y.resize(slots, 0);
        self.speed.resize(slots, 0);
        self.dest.resize(slots, Pos2::new(Fixed::ZERO, Fixed::ZERO));
        self.horizon.resize(slots, 0);
        self.wait.resize(slots, 0);
        self.dwell.resize(slots, 0);
        self.home.resize(slots, None);
        self.phase_at.resize(slots, false);
        self.health.resize(slots, HEALTH_MAX);
        self.dies_at.resize(slots, u32::MAX);
    }
}

/// Which cell a position falls in, by umwelt's own arithmetic, flattened.
fn cell_index(cfg: &WorldConfig, x: Fixed, y: Fixed, per_axis: usize) -> usize {
    let c = cfg.cell_of(Pos2::new(x, y));
    c.y as usize * per_axis + c.x as usize
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
    subs_changed: u64,
}

impl Totals {
    fn add(&mut self, s: &TickStats) {
        self.viewers += s.viewers;
        self.candidates += s.candidates;
        self.records += s.records;
        self.bytes += s.bytes;
        self.subs_changed += s.subs_changed;
    }
}

/// How the population sits in cells. The checkpoint for movement: whether
/// crowds form on their own.
///
/// Counted by this game rather than read out of umwelt. The cell-ordered
/// snapshot is umwelt's own working state and not something a consumer is
/// handed, and this game already tallies its own crowd every tick to steer
/// away from full cells.
struct Occupancy {
    max: usize,
}

fn occupancy<S: PayloadSink>(sim: &WorldSimulation<Herd, S>) -> Occupancy {
    let counts = &sim.game().occupancy;
    Occupancy { max: counts.iter().copied().max().unwrap_or(0) as usize }
}

fn main() {
    let mut ticks = TICKS;
    let mut wait = Wait::Sleep;
    let mut threads = None;
    let mut entities = ENTITIES;
    let mut viewers = VIEWERS;
    let mut damage = DAMAGE_DIVISOR;
    let mut crowd = CROWD_THRESHOLD;
    let mut lifespan = LIFESPAN_S;
    let mut pattern = Pattern::Herd;
    let mut nats = herd_common::DEFAULT_NATS.to_string();
    let mut region = 7u32;
    let mut heartbeat = 30u64;
    let mut edge_timeout = 5u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--free-run" => wait = Wait::None,
            "--hold" => wait = Wait::Hold,
            "--threads" => threads = args.next().and_then(|n| n.parse().ok()),
            "--entities" => {
                entities = args.next().and_then(|n| n.parse().ok()).unwrap_or(entities)
            }
            "--viewers" => {
                viewers = args.next().and_then(|n| n.parse().ok()).unwrap_or(viewers)
            }
            "--damage" => {
                damage = args.next().and_then(|n| n.parse().ok()).unwrap_or(damage)
            }
            "--crowd" => {
                crowd = args.next().and_then(|n| n.parse().ok()).unwrap_or(crowd)
            }
            "--lifespan" => {
                lifespan = args.next().and_then(|n| n.parse().ok()).unwrap_or(lifespan)
            }
            // Nothing here decides where the broker is or what the region is
            // called: a deployment does, which is why they are arguments.
            "--nats" => nats = args.next().unwrap_or(nats),
            "--region" => {
                region = args.next().and_then(|n| n.parse().ok()).unwrap_or(region)
            }
            "--heartbeat" => {
                heartbeat = args.next().and_then(|n| n.parse().ok()).unwrap_or(heartbeat)
            }
            "--edge-timeout" => {
                edge_timeout =
                    args.next().and_then(|n| n.parse().ok()).unwrap_or(edge_timeout)
            }
            "--pattern" => {
                pattern =
                    args.next().as_deref().and_then(Pattern::parse).unwrap_or_else(|| {
                        eprintln!(
                            "--pattern takes herd, flash, flap, thrash, teleport or cull"
                        );
                        std::process::exit(2)
                    })
            }
            other => match other.parse() {
                Ok(n) => ticks = n,
                Err(_) => {
                    eprintln!(
                        "usage: herd-sim [ticks] [--free-run|--hold] [--threads N] \
                         [--entities N] [--viewers N] [--nats URL] [--region N]"
                    );
                    std::process::exit(2)
                }
            },
        }
    }
    let report_every = (ticks / REPORTS).max(1);

    let cfg = herd_common::world(TICK_HZ);
    let region = RegionId::from_raw(region);

    // This binary owns its connection, so the broker address, credentials and
    // cluster membership are decided here rather than by umwelt.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let client = runtime
        .block_on(herd_common::connect(&nats, herd_common::arg("creds")))
        .unwrap_or_else(|e| {
            eprintln!("nats {nats}: {e}");
            std::process::exit(1);
        });
    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    // Held for the whole run: dropping it stops serving.
    let server = RegionServer::new(
        client.clone(),
        runtime.handle().clone(),
        region,
        cfg,
        Arc::clone(&inbound),
        Duration::from_secs(edge_timeout),
    )
    .unwrap_or_else(|e| {
        eprintln!("serving {region}: {e}");
        std::process::exit(1);
    });
    // Cadence is a deployment choice; the timer is umwelt's.
    server.set_heartbeat_interval(Duration::from_secs(heartbeat));
    let sink =
        EdgeSink::new(region, client, runtime.handle().clone(), Arc::clone(&edges));

    let mut game = Herd::new(
        &cfg,
        entities,
        SEED,
        lifespan * cfg.tick_hz(),
        pattern,
        Arc::clone(&inbound),
    );
    game.damage_divisor = damage;
    game.crowd_threshold = crowd;
    let mut sim = WorldSimulation::new(cfg, game).with_sink(Handoff::new(sink.clone()));
    if let Some(n) = threads {
        sim.set_thread_count(n);
    }

    // The first tick spawns, so there is nothing to attach a client to before
    // it. Registration is logical: umwelt never sees a socket.
    sim.tick();
    let avatars: Vec<u32> =
        sim.game().residents().iter().copied().take(viewers).collect();
    let registered = avatars.len();
    for e in avatars {
        sim.register_viewer(EntityId::from_raw(e), herd_common::limits());
    }

    println!("herd-sim: serving {region} over {nats}");
    println!(
        "herd-sim: {pattern:?}, {entities} entities, {} m region, {} m cells, {} Hz, {} workers",
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
    println!(
        "{registered} clients, drawn from the entities that move and have a home, \
         each declaring a {}-byte payload.",
        herd_common::PAYLOAD_BYTES
    );
    println!();
    println!(
        "{:>6} {:>10} {:>9} {:>8} {:>8} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6}",
        "tick",
        "phase",
        "max cell",
        "alive",
        "deaths",
        "cand",
        "rec",
        "dep",
        "desp",
        "work ms",
        "edges"
    );

    let period = std::time::Duration::from_nanos(1_000_000_000 / cfg.tick_hz() as u64);
    let mut totals = Totals::default();
    let mut work = Histogram::new();
    let mut late = Histogram::new();
    let mut over_budget = 0u64;
    let mut over_half = 0u64;
    let mut over_three_quarters = 0u64;
    // Worst tick since the last report line, so a spike can be placed against
    // the phase it happened in rather than only counted at the end.
    let mut window_worst = std::time::Duration::ZERO;
    let mut peak_cell = 0usize;

    // umwelt owns the loop: it has the tick rate, so it keeps the schedule.
    // Zero runs until interrupted, which is what a region with edges attached
    // wants; a fixed count is what a measurement run wants.
    let limit = (ticks > 0).then_some(ticks);
    let summary = sim.run(
        Pacing {
            wait,
            ticks: limit,
            ..Pacing::default()
        },
        |r, sim| {
            // Between ticks is the only place a viewer can be registered or
            // dropped, which is why this is here and not in the game.
            inbound.settle(sim, &sink, herd_common::limits());
            work.record(r.took.as_micros() as u64);
            if !r.late.is_zero() {
                late.record(r.late.as_micros() as u64);
            }
            if r.took > period {
                over_budget += 1;
            }
            if r.took * 2 > period {
                over_half += 1;
            }
            if r.took * 4 > period * 3 {
                over_three_quarters += 1;
            }
            window_worst = window_worst.max(r.took);
            totals.add(&r.stats);

            if r.tick % report_every == 0 {
                let o = occupancy(sim);
                let phase = if gathering(r.tick, (PHASE_S * cfg.tick_hz() as i32) as u32) {
                    "gathering"
                } else {
                    "dispersing"
                };
                peak_cell = peak_cell.max(o.max);
                let served = r.stats.viewers.max(1) as f64;
                println!(
                    "{:>6} {:>10} {:>9} {:>8} {:>8} {:>7.1} {:>7.1} {:>7.2} {:>7.2} {:>8.2} {:>6}",
                    r.tick,
                    phase,
                    o.max,
                    sim.entity_count(),
                    sim.game().deaths,
                    r.stats.candidates as f64 / served,
                    r.stats.records as f64 / served,
                    r.stats.departed as f64 / served,
                    r.stats.despawns_sent as f64 / served,
                    r.took.as_secs_f64() * 1000.0,
                    edges.len(),
                );
                window_worst = std::time::Duration::ZERO;
            }
            Flow::Continue
        },
    );

    let budget_ms = period.as_secs_f64() * 1000.0;
    println!();
    // Wall clock per tick is the period, not the work: a paced loop sleeps
    // until it is. Only the work is measured against the budget.
    match wait {
        Wait::None => println!(
            "{} ticks in {:.2} s, unpaced, so this is throughput and not a schedule",
            summary.ticks,
            summary.elapsed.as_secs_f64()
        ),
        _ => println!(
            "{} ticks at {} Hz in {:.2} s, against a {:.2} s schedule plus the last tick's work",
            summary.ticks,
            cfg.tick_hz(),
            summary.elapsed.as_secs_f64(),
            period.as_secs_f64() * summary.ticks as f64
        ),
    }
    println!(
        "tick work, which is what has to fit: p50 {:.2}, p90 {:.2}, p99 {:.2}, p99.9 {:.2}, worst {:.2} ms of a {budget_ms:.0} ms budget",
        work.quantile_us(0.50) as f64 / 1000.0,
        work.quantile_us(0.90) as f64 / 1000.0,
        work.quantile_us(0.99) as f64 / 1000.0,
        work.quantile_us(0.999) as f64 / 1000.0,
        summary.worst_tick.as_secs_f64() * 1000.0
    );
    println!(
        "of {} ticks: {} over half the budget, {} over three quarters, {} over it",
        summary.ticks, over_half, over_three_quarters, over_budget
    );
    if wait != Wait::None {
        println!(
            "schedule: {} of {} ticks started after their deadline, p99 lateness {:.2} ms, worst {:.2} ms, {} deadlines dropped",
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
    println!(
        "subscriptions: {} changed, {:.2}% of viewers served, which is how often one crossed a cell",
        totals.subs_changed,
        100.0 * totals.subs_changed as f64 / totals.viewers.max(1) as f64
    );
    let seconds = summary.ticks as f64 / cfg.tick_hz() as f64;
    println!(
        "population: {} alive of {entities}, {} died and {} spawned, {:.1} deaths per second, {} slots ever allocated",
        sim.entity_count(),
        sim.game().deaths,
        sim.game().births,
        sim.game().deaths as f64 / seconds.max(1.0),
        sim.game().slots()
    );
    println!(
        "crowding: fullest cell peaked at {peak_cell} and ended at {}, against a threshold of {crowd}",
        occupancy(&sim).max
    );
    println!(
        "edges: {} attached, {} payloads delivered and {} dropped by the handoff, \
         {} undeliverable, {} commands refused",
        edges.len(),
        sim.sink().delivered(),
        sim.sink().dropped(),
        sink.undeliverable(),
        inbound.refused(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_alternates_between_gathering_and_dispersing() {
        // Every entity sees the same phase at the same tick, which is what
        // makes crowd sizes repeat from one cycle to the next.
        assert!(gathering(0, 10));
        assert!(gathering(9, 10));
        assert!(!gathering(10, 10));
        assert!(!gathering(19, 10));
        assert!(gathering(20, 10));
    }

    #[test]
    fn a_run_reproduces_from_its_seed() {
        let draw = |seed| {
            let mut rng = Rng::new(seed);
            (0..64).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(SEED), draw(SEED), "the same seed must replay a run");
        assert_ne!(draw(SEED), draw(SEED + 2));
    }

    #[test]
    fn an_even_seed_draws_the_same_run_as_the_odd_one_above_it() {
        // `Rng::new` sets the low bit, because xorshift64 is stuck at zero and
        // an all-even state would halve the period. The cost is that the seed
        // space is halved too: 2n and 2n+1 are one stream. Harmless for a load
        // generator, and worth knowing before anyone sweeps seeds by one.
        let draw = |seed| {
            let mut rng = Rng::new(seed);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(2), draw(3));
    }

    #[test]
    fn a_spread_stays_inside_its_bound() {
        let mut rng = Rng::new(SEED);
        for _ in 0..10_000 {
            let n = rng.spread(64);
            assert!((-64..=64).contains(&n), "{n} is outside the bound");
        }
    }

    #[test]
    fn a_histogram_reports_quantiles_in_order() {
        let mut h = Histogram::new();
        for us in 1..=1_000u64 {
            h.record(us);
        }
        // Log-spaced buckets, so a quantile names the bucket it landed in
        // rather than interpolating. What has to hold is the ordering and the
        // scale, not an exact value.
        let (p50, p90, p99) =
            (h.quantile_us(0.50), h.quantile_us(0.90), h.quantile_us(0.99));
        assert!(p50 <= p90 && p90 <= p99, "quantiles came out {p50}, {p90}, {p99}");
        assert!((256..=1_024).contains(&p50), "p50 came out at {p50}");
        assert!((512..=1_024).contains(&p99), "p99 came out at {p99}");
    }

    #[test]
    fn one_value_lands_within_a_quarter_octave_of_itself() {
        let mut h = Histogram::new();
        h.record(1_000);
        let got = h.quantile_us(0.5);
        assert!(
            (840..=1_190).contains(&got),
            "a single 1000 us tick reported as {got}, further than a bucket away"
        );
    }

    #[test]
    fn an_empty_histogram_has_no_quantile() {
        assert_eq!(Histogram::new().quantile_us(0.5), 0);
    }
}
