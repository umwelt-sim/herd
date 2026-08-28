//! The game itself: entities, where they are, and where they walk.
//!
//! None of this is umwelt's. A real game would have terrain, intent and rules
//! here; herd has a column of walkers that pace back and forth, which is enough
//! to make a region do its work.
//!
//! Entities are named by the handle [`ClientHandle::spawn`] returned. A handle
//! names one from the moment it is asked for, so this walks and moves entities
//! before any region has said what id it gave them.

use umwelt::net::{ClientHandle, EntityKind, NetError};
use umwelt::{EntityId, Fixed, Pos3, RegionId};

use crate::watcher::Reports;

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

/// One entity this game asked for.
struct Held {
    handle: u32,
    /// Which region it was asked for in. A game is the only tier that sees more
    /// than one at a time, so keeping this is its job.
    region: RegionId,
    at: Pos3,
    heading: i32,
    /// The region's own id, once the edge has said what it is. Until then the
    /// handle is the only name for it, which is the point of having one.
    entity: Option<(RegionId, EntityId)>,
}

/// Everything this game is holding.
pub struct Crowd {
    held: Vec<Held>,
    /// The column this game's entities live in, so several games do not stack
    /// on one spot.
    lane: i32,
    /// How far a walker travels in one send.
    step: Fixed,
    /// Where the next entity starts.
    next: usize,
    /// Where a new entity goes, and one side of the pair a migration walks
    /// between.
    home: RegionId,
    /// Where migration sends them, if this game was told to walk them at all.
    away: Option<RegionId>,
    /// How far the migration sweep has got. Taking a fixed prefix would walk
    /// the same few back and forth while the rest never moved.
    cursor: usize,
    pub migrated: u64,
}

impl Crowd {
    /// Asks the edge for a population and starts holding it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sending: &ClientHandle,
        home: RegionId,
        away: Option<RegionId>,
        lane: i32,
        send_hz: u32,
        observers: usize,
        unattended: usize,
    ) -> Result<Crowd, NetError> {
        let mut crowd = Crowd {
            held: Vec::with_capacity(observers + unattended),
            lane,
            step: Fixed::from_raw(
                Fixed::from_meters(WALK_M_PER_SEC).raw() / send_hz.max(1) as i32,
            ),
            next: 0,
            home,
            away,
            cursor: 0,
            migrated: 0,
        };
        for k in 0..observers + unattended {
            let kind =
                if k < observers { EntityKind::Observer } else { EntityKind::Unattended };
            crowd.ask_for_one(sending, kind)?;
        }
        Ok(crowd)
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// How many the regions have given ids to.
    pub fn confirmed(&self) -> usize {
        self.held.iter().filter(|h| h.entity.is_some()).count()
    }

    /// How many are somewhere other than home.
    pub fn away(&self) -> usize {
        self.held.iter().filter(|h| h.region != self.home).count()
    }

    /// Applies what the edge has said since the last pass.
    pub fn settle(&mut self, reports: &Reports) {
        for (handle, region, entity) in reports.take_spawned() {
            if let Some(one) = self.held.iter_mut().find(|h| h.handle == handle) {
                one.entity = Some((region, entity));
                one.region = region;
            }
        }
        // Anything the edge says is gone stops being moved, however it went —
        // including a despawn this game never asked for, because a region's own
        // game can despawn anything.
        for handle in reports.take_removed() {
            self.held.retain(|h| h.handle != handle);
        }
    }

    /// Walks everything one send's worth and tells the edge where it all is.
    pub fn walk(&mut self, sending: &ClientHandle) -> Result<usize, NetError> {
        for one in self.held.iter_mut() {
            let moved = Fixed::from_raw(one.at.x.raw() + self.step.raw() * one.heading);
            if (moved.floor_meters() - self.lane).abs() > RANGE_M {
                one.heading = -one.heading;
            } else {
                one.at.x = moved;
            }
        }
        let moves: Vec<(u32, Pos3)> = self.held.iter().map(|h| (h.handle, h.at)).collect();
        if moves.is_empty() {
            return Ok(0);
        }
        sending.move_entities(&moves)?;
        Ok(moves.len())
    }

    /// Hands back `n` entities and asks for that many replacements, which is
    /// what game clients coming and going looks like from here.
    pub fn churn(&mut self, sending: &ClientHandle, n: usize) -> Result<usize, NetError> {
        if n == 0 || self.held.len() < n {
            return Ok(0);
        }
        let leaving: Vec<u32> = self.held.iter().rev().take(n).map(|h| h.handle).collect();
        for handle in &leaving {
            sending.despawn(*handle)?;
        }
        // Dropped here rather than when the edge confirms. umwelt has already
        // forgotten them, so anything this crowd did with them afterwards —
        // moving them, choosing one to migrate — would be naming an entity that
        // is not there.
        self.held.retain(|h| !leaving.contains(&h.handle));
        for _ in 0..leaving.len() {
            self.ask_for_one(sending, EntityKind::Observer)?;
        }
        Ok(leaving.len())
    }

    /// Walks some of the crowd into the other region, and some of it back.
    ///
    /// The whole of ad hoc migration: ask the destination for it, and give the
    /// origin's copy back when the destination has it. `umwelt` does both
    /// halves, so this only says which entity and where. See `docs/adr/0003`.
    pub fn migrate(&mut self, sending: &ClientHandle, n: usize) -> Result<(), NetError> {
        let Some(away) = self.away else { return Ok(()) };
        if n == 0 || self.held.is_empty() {
            return Ok(());
        }
        // A cursor sweeping the crowd rather than its front: an entity that has
        // just arrived is at the back, and taking a fixed prefix would walk the
        // same few back and forth while the rest never moved.
        let mut going = Vec::with_capacity(n.min(self.held.len()));
        for _ in 0..self.held.len().min(n) {
            self.cursor = (self.cursor + 1) % self.held.len();
            let one = &self.held[self.cursor];
            // Only entities the regions have answered for: one still in flight
            // has no id to give back.
            if one.entity.is_some() {
                going.push(self.cursor);
            }
        }
        for at in going {
            let (handle, from, position) =
                (self.held[at].handle, self.held[at].region, self.held[at].at);
            let there = if from == self.home { away } else { self.home };
            // The same coordinates in the other region: both are 4096 m, so a
            // door is wherever the game says it is.
            let moved = sending.migrate(handle, there, position)?;
            self.held[at] = Held {
                handle: moved,
                region: there,
                at: position,
                heading: self.held[at].heading,
                entity: None,
            };
            self.migrated += 1;
        }
        Ok(())
    }

    fn ask_for_one(
        &mut self,
        sending: &ClientHandle,
        kind: EntityKind,
    ) -> Result<(), NetError> {
        let at = Pos3::from_meters(self.lane, 64 + (self.next as i32 % 3072), 0);
        self.next += 1;
        let handle = sending.spawn(self.home, at, kind)?;
        self.held.push(Held { handle, region: self.home, at, heading: 1, entity: None });
        Ok(())
    }
}
