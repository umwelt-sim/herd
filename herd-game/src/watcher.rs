//! What the edge tells this game.
//!
//! One of the two halves a game developer writes. [`Watcher`] implements
//! [`ClientGame`], which umwelt calls: nothing here polls, waits, or decides
//! what a timeout means.
//!
//! The calls arrive on umwelt's I/O threads, so they must not block. This one
//! puts what it was told somewhere the game loop can pick it up and returns.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use umwelt::{ClientGame, EntityId, PacketReader, RegionId};

/// What the edge has said, waiting for the loop in [`crate::session`].
///
/// Cheap to clone: one copy goes to the [`Watcher`] umwelt calls, one stays
/// with the loop that reads it.
#[derive(Clone, Default)]
pub struct Reports {
    spawned: Arc<Mutex<Vec<(u32, RegionId, EntityId)>>>,
    removed: Arc<Mutex<Vec<u32>>>,
    packets: Arc<AtomicU64>,
    records: Arc<AtomicU64>,
    gone: Arc<AtomicBool>,
}

impl Reports {
    /// Entities the regions have now allocated ids for.
    pub fn take_spawned(&self) -> Vec<(u32, RegionId, EntityId)> {
        std::mem::take(&mut *self.spawned.lock().expect("not poisoned"))
    }

    /// Entities that have gone, however they went.
    pub fn take_removed(&self) -> Vec<u32> {
        std::mem::take(&mut *self.removed.lock().expect("not poisoned"))
    }

    /// Packets and records since this was last asked.
    pub fn take_traffic(&self) -> (u64, u64) {
        (self.packets.swap(0, Ordering::Relaxed), self.records.swap(0, Ordering::Relaxed))
    }

    /// Whether the connection has gone.
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    pub fn watcher(&self) -> Watcher {
        Watcher { reports: self.clone() }
    }
}

/// The game's receive side. Everything umwelt has to say arrives here.
pub struct Watcher {
    reports: Reports,
}

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: u32, region: RegionId, entity: EntityId) {
        self.reports.spawned.lock().expect("not poisoned").push((handle, region, entity));
    }

    fn removed(&mut self, handle: u32) {
        self.reports.removed.lock().expect("not poisoned").push(handle);
    }

    fn state(&mut self, _handle: u32, _region: RegionId, state: &PacketReader<'_>) {
        // Already decoded: no packet, no codec, and nothing about how the
        // region that built it was configured. A real game would put these
        // positions into whatever it draws from.
        self.reports.packets.fetch_add(1, Ordering::Relaxed);
        self.reports.records.fetch_add(state.updates().count() as u64, Ordering::Relaxed);
    }

    fn disconnected(&mut self) {
        self.reports.gone.store(true, Ordering::Relaxed);
    }
}
