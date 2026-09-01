//! The game itself: entities, where they are, and where they walk.
//!
//! None of this is umwelt's. A real game would have terrain, intent and rules
//! here; herd has a column of walkers that pace back and forth, which is enough
//! to make a region do its work.
//!
//! Entities are named by the handle [`Link::spawn`](crate::link::Link::spawn) returned. A handle
//! names one from the moment it is asked for, so this walks and moves entities
//! before any region has said what id it gave them.

use umwelt::{EntityHandle, EntityId, EntityKind, Fixed, NetError, Pos3, RegionId};

use crate::link::Link;
use crate::watcher::Reports;

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

/// One entity this game asked for.
struct Held {
    handle: EntityHandle,
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
        sending: &impl Link,
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
                if k < observers { EntityKind::observer(0) } else { EntityKind::unattended(0) };
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
    pub fn walk(&mut self, sending: &impl Link) -> Result<usize, NetError> {
        for one in self.held.iter_mut() {
            let moved = Fixed::from_raw(one.at.x.raw() + self.step.raw() * one.heading);
            if (moved.floor_meters() - self.lane).abs() > RANGE_M {
                one.heading = -one.heading;
            } else {
                one.at.x = moved;
            }
        }
        let moves: Vec<(EntityHandle, Pos3)> =
            self.held.iter().map(|h| (h.handle, h.at)).collect();
        if moves.is_empty() {
            return Ok(0);
        }
        sending.move_entities(&moves)?;
        Ok(moves.len())
    }

    /// Hands back `n` entities and asks for that many replacements, which is
    /// what game clients coming and going looks like from here.
    pub fn churn(&mut self, sending: &impl Link, n: usize) -> Result<usize, NetError> {
        if n == 0 || self.held.len() < n {
            return Ok(0);
        }
        let leaving: Vec<EntityHandle> =
            self.held.iter().rev().take(n).map(|h| h.handle).collect();
        for handle in &leaving {
            sending.despawn(*handle)?;
        }
        // Dropped here rather than when the edge confirms. umwelt has already
        // forgotten them, so anything this crowd did with them afterwards —
        // moving them, choosing one to migrate — would be naming an entity that
        // is not there.
        self.held.retain(|h| !leaving.contains(&h.handle));
        for _ in 0..leaving.len() {
            self.ask_for_one(sending, EntityKind::observer(0))?;
        }
        Ok(leaving.len())
    }

    /// Walks some of the crowd into the other region, and some of it back.
    ///
    /// The whole of ad hoc migration: ask the destination for it, and give the
    /// origin's copy back when the destination has it. `umwelt` does both
    /// halves, so this only says which entity and where.
    pub fn migrate(&mut self, sending: &impl Link, n: usize) -> Result<(), NetError> {
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
        sending: &impl Link,
        kind: EntityKind,
    ) -> Result<(), NetError> {
        let at = Pos3::from_meters(self.lane, 64 + (self.next as i32 % 3072), 0);
        self.next += 1;
        let handle = sending.spawn(self.home, at, kind)?;
        self.held.push(Held { handle, region: self.home, at, heading: 1, entity: None });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand, since every assertion in here names one.
    fn h(raw: u32) -> EntityHandle {
        EntityHandle::from_raw(raw)
    }
    use crate::link::fake::{Fake, Said};

    const HOME: RegionId = RegionId::from_raw(7);
    const AWAY: RegionId = RegionId::from_raw(8);

    fn crowd(link: &Fake, n: usize, away: Option<RegionId>) -> Crowd {
        Crowd::new(link, HOME, away, 100, 20, n, 0).expect("asks for its population")
    }

    /// Everything the edge says about an entity, as a `Reports` would carry it.
    fn confirm(crowd: &mut Crowd, handles: &[(EntityHandle, RegionId)]) {
        let reports = Reports::default();
        for (n, (handle, region)) in handles.iter().enumerate() {
            reports.saw_spawned(*handle, *region, EntityId::from_raw(n as u32));
        }
        crowd.settle(&reports);
    }

    #[test]
    fn asking_for_a_population_asks_once_each_and_holds_them_all() {
        let link = Fake::new();
        let crowd = crowd(&link, 4, None);
        assert_eq!(crowd.len(), 4);
        assert_eq!(crowd.confirmed(), 0, "no region has answered yet");
        assert_eq!(link.said().len(), 4);
        assert!(link.said().iter().all(|s| matches!(s, Said::Spawn(r, _) if *r == HOME)));
    }

    #[test]
    fn an_entity_is_moved_before_any_region_has_named_it() {
        // A handle names an entity from the moment it is asked for. That is the
        // whole point of having one, and it hides a round trip.
        let link = Fake::new();
        let mut crowd = crowd(&link, 3, None);
        assert_eq!(crowd.confirmed(), 0);
        assert_eq!(crowd.walk(&link).expect("moves"), 3);
        let said = link.said();
        assert!(matches!(said.last(), Some(Said::Moved(m)) if m.len() == 3));
    }

    #[test]
    fn walking_turns_around_at_the_edge_of_its_range() {
        let link = Fake::new();
        let mut crowd = crowd(&link, 1, None);
        // Far enough that it must have reversed at least once.
        for _ in 0..(RANGE_M as usize * 40) {
            crowd.walk(&link).expect("moves");
        }
        let x = crowd.held[0].at.x.floor_meters();
        assert!(
            (x - 100).abs() <= RANGE_M + 1,
            "walked to {x}, outside {RANGE_M} m of its lane"
        );
    }

    #[test]
    fn an_entity_the_edge_reports_gone_stops_being_held() {
        let link = Fake::new();
        let mut crowd = crowd(&link, 3, None);
        let reports = Reports::default();
        reports.saw_removed(h(2));
        crowd.settle(&reports);
        assert_eq!(crowd.len(), 2, "a despawn nobody asked for still removes it");
    }

    #[test]
    fn churn_gives_some_back_and_asks_for_that_many_more() {
        let link = Fake::new();
        let mut crowd = crowd(&link, 4, None);
        assert_eq!(crowd.churn(&link, 2).expect("churns"), 2);
        assert_eq!(crowd.len(), 4, "as many as before, two of them new");
        let said = link.said();
        assert_eq!(said.iter().filter(|s| matches!(s, Said::Despawn(_))).count(), 2);
        assert_eq!(said.iter().filter(|s| matches!(s, Said::Spawn(..))).count(), 6);
    }

    #[test]
    fn a_churned_entity_is_dropped_at_once_rather_than_when_the_edge_confirms() {
        // umwelt forgets a handle the moment it is given back, so anything this
        // crowd did with it afterwards would name an entity that is not there.
        // Leaving them in was what made migration kill the client.
        let link = Fake::new();
        let mut crowd = crowd(&link, 4, None);
        let given_back = match link.said()[0] {
            Said::Spawn(_, handle) => handle,
            _ => unreachable!(),
        };
        crowd.churn(&link, 4).expect("churns");
        assert!(
            !crowd.held.iter().any(|h| h.handle == given_back),
            "an entity given back is still being held"
        );
    }

    #[test]
    fn churn_and_migration_together_do_not_name_a_forgotten_entity() {
        // The pair that broke: churn gave entities back, migration then picked
        // one of them, and the error stopped the client.
        let link = Fake::new();
        let mut crowd = crowd(&link, 8, Some(AWAY));
        let handles: Vec<(EntityHandle, RegionId)> =
            (1..=8).map(|h| (EntityHandle::from_raw(h), HOME)).collect();
        confirm(&mut crowd, &handles);
        for _ in 0..8 {
            crowd.churn(&link, 3).expect("churns");
            crowd.migrate(&link, 3).expect("migrates without naming a ghost");
        }
    }

    #[test]
    fn migration_walks_them_to_the_other_region_and_back() {
        let link = Fake::new();
        let mut crowd = crowd(&link, 2, Some(AWAY));
        confirm(&mut crowd, &[(h(1), HOME), (h(2), HOME)]);

        crowd.migrate(&link, 2).expect("migrates");
        assert_eq!(crowd.migrated, 2);
        assert_eq!(crowd.away(), 2, "both are in the other region now");
        let went: Vec<RegionId> = link
            .said()
            .iter()
            .filter_map(|s| match s {
                Said::Migrate { to, .. } => Some(*to),
                _ => None,
            })
            .collect();
        assert_eq!(went, vec![AWAY, AWAY]);

        // The handles changed, so the crowd has to be holding the new ones.
        let now: Vec<(EntityHandle, RegionId)> =
            crowd.held.iter().map(|h| (h.handle, AWAY)).collect();
        confirm(&mut crowd, &now);
        crowd.migrate(&link, 2).expect("migrates back");
        assert_eq!(crowd.away(), 0, "and home again");
    }

    #[test]
    fn migration_skips_an_entity_no_region_has_answered_for() {
        // One still in flight has no id to give back, so there is nothing to
        // migrate yet.
        let link = Fake::new();
        let mut crowd = crowd(&link, 4, Some(AWAY));
        confirm(&mut crowd, &[(h(1), HOME), (h(2), HOME)]);
        crowd.migrate(&link, 4).expect("migrates");
        assert_eq!(crowd.migrated, 2, "only the two with ids");
    }

    #[test]
    fn a_crowd_told_of_nowhere_else_never_migrates() {
        let link = Fake::new();
        let mut crowd = crowd(&link, 4, None);
        confirm(&mut crowd, &[(h(1), HOME), (h(2), HOME), (h(3), HOME), (h(4), HOME)]);
        crowd.migrate(&link, 4).expect("does nothing");
        assert_eq!(crowd.migrated, 0);
        assert_eq!(crowd.away(), 0);
    }
}
