// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![deny(rustdoc::broken_intra_doc_links)]

#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#![allow(dead_code)] // TODO: Remove when complete

use core::hash;
use std::sync::{Arc, Mutex};

use lightning::ln::peer_handler::SocketDescriptor;

/// An synchronous SocketDescriptor (i.e. it doesn't rely on Tokio)
#[derive(Clone)]
struct SyncSocketDescriptor {
    id: u16,
    outbound_data: Arc<Mutex<Vec<u8>>>,
}
impl PartialEq for SyncSocketDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for SyncSocketDescriptor { }
impl hash::Hash for SyncSocketDescriptor {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl SocketDescriptor for SyncSocketDescriptor {
    fn send_data(&mut self, _data: &[u8], _resume_read: bool) -> usize {
        unimplemented!();
    }

    fn disconnect_socket(&mut self) {
        unimplemented!();
    }
}

#[cfg(test)]
mod tests {
    fn hello_test() {
        assert!(true);
    }
}
