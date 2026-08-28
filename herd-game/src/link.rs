//! What a crowd needs from umwelt, and nothing else.
//!
//! Four calls, forwarded straight to [`ClientHandle`]. It exists so
//! [`Crowd`](crate::crowd::Crowd) can be exercised without a connection: the
//! game's own logic — who moves where, who is replaced, who walks into the
//! other region — is worth testing on its own, and none of it needs a socket.
//!
//! A real game would likely hold a `ClientHandle` directly. This is here
//! because the alternative was leaving two hundred lines of logic untested.

use umwelt::net::{ClientHandle, EntityKind, NetError};
use umwelt::{Pos3, RegionId};

/// The part of umwelt a crowd speaks to.
pub trait Link {
    /// Asks a region for an entity, and returns the handle it goes by.
    fn spawn(&self, region: RegionId, at: Pos3, kind: EntityKind) -> Result<u32, NetError>;

    /// Moves an entity to another region, returning the handle it goes by
    /// there. The old one stops working once the move lands.
    fn migrate(&self, handle: u32, to: RegionId, at: Pos3) -> Result<u32, NetError>;

    fn despawn(&self, handle: u32) -> Result<(), NetError>;

    fn move_entities(&self, moves: &[(u32, Pos3)]) -> Result<(), NetError>;
}

impl Link for ClientHandle {
    fn spawn(&self, region: RegionId, at: Pos3, kind: EntityKind) -> Result<u32, NetError> {
        ClientHandle::spawn(self, region, at, kind)
    }

    fn migrate(&self, handle: u32, to: RegionId, at: Pos3) -> Result<u32, NetError> {
        ClientHandle::migrate(self, handle, to, at)
    }

    fn despawn(&self, handle: u32) -> Result<(), NetError> {
        ClientHandle::despawn(self, handle)
    }

    fn move_entities(&self, moves: &[(u32, Pos3)]) -> Result<(), NetError> {
        ClientHandle::move_entities(self, moves)
    }
}

#[cfg(test)]
pub mod fake {
    //! A link that remembers what it was told and hands out handles in order.

    use std::cell::RefCell;

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    pub enum Said {
        Spawn(RegionId, u32),
        Migrate { handle: u32, to: RegionId, became: u32 },
        Despawn(u32),
        Moved(Vec<u32>),
    }

    #[derive(Default)]
    pub struct Fake {
        next: RefCell<u32>,
        /// Handles it will refuse, standing in for entities umwelt has already
        /// forgotten.
        pub gone: RefCell<Vec<u32>>,
        pub said: RefCell<Vec<Said>>,
    }

    impl Fake {
        pub fn new() -> Fake {
            Fake::default()
        }

        fn mint(&self) -> u32 {
            let mut next = self.next.borrow_mut();
            *next += 1;
            *next
        }

        fn holds(&self, handle: u32) -> Result<(), NetError> {
            if self.gone.borrow().contains(&handle) {
                return Err(NetError::Unknown("handle"));
            }
            Ok(())
        }

        pub fn said(&self) -> std::cell::Ref<'_, Vec<Said>> {
            self.said.borrow()
        }

        pub fn forget(&self, handle: u32) {
            self.gone.borrow_mut().push(handle);
        }
    }

    impl Link for Fake {
        fn spawn(&self, region: RegionId, _at: Pos3, _kind: EntityKind) -> Result<u32, NetError> {
            let handle = self.mint();
            self.said.borrow_mut().push(Said::Spawn(region, handle));
            Ok(handle)
        }

        fn migrate(&self, handle: u32, to: RegionId, _at: Pos3) -> Result<u32, NetError> {
            self.holds(handle)?;
            let became = self.mint();
            self.said.borrow_mut().push(Said::Migrate { handle, to, became });
            Ok(became)
        }

        fn despawn(&self, handle: u32) -> Result<(), NetError> {
            self.holds(handle)?;
            self.forget(handle);
            self.said.borrow_mut().push(Said::Despawn(handle));
            Ok(())
        }

        fn move_entities(&self, moves: &[(u32, Pos3)]) -> Result<(), NetError> {
            self.said
                .borrow_mut()
                .push(Said::Moved(moves.iter().map(|(h, _)| *h).collect()));
            Ok(())
        }
    }
}
