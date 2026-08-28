//! What this edge does of its own accord, which is almost nothing.
//!
//! Every callback has a default that does nothing, and a game whose clients
//! only spawn, move and despawn implements none of them. These two are here to
//! say who is connected.
//!
//! There is no herd here. An edge is a relay: what walks around a region
//! belongs to a game, and walking between two of them is a game's to perform —
//! see `herd-game --to`. An edge keeping entities of its own would be a
//! simulation in the wrong tier.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use umwelt::{ClientId, EdgeGame};

pub struct Game {
    pub clients: Arc<Mutex<Vec<ClientId>>>,
}

impl EdgeGame for Game {
    fn connected(&mut self, client: ClientId, from: SocketAddr) {
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
}
