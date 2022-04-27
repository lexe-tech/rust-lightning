// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! A socket handling library for those using rust-lightning within a synchronous,
//! multi-threaded runtime, including inside SGX.
//!
//! Whereas `lightning-net-tokio` manages reading and writing to peers using
//! Futures and Tokio tasks, this library uses dedicated blocking threads. While
//! this does result in a small amount of performance overhead, the complete
//! absence of any Tokio `net` features means that this library can successfully
//! compile and run with the `x86_64-fortanix-unknown-sgx` (EDP) target.
//! See the [EDP docs](https://edp.fortanix.com/docs/concepts/rust-std/) for
//! more information on what Rust features can and cannot be used within SGX.
//!
//! Those who wish to maximize performance should use `lightning-net-tokio`.
//! Those who want to run rust-lightning with a synchronous runtime, smaller
//! code size, or no dependency or Tokio, should use this crate.
//!
//! # Example Usage
//! # TODO

#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::type_complexity)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![allow(dead_code)] // TODO: Remove when complete

use core::hash;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};

use lightning::ln::msgs::{ChannelMessageHandler, NetAddress, RoutingMessageHandler};
use lightning::ln::peer_handler::{
    CustomMessageHandler, PeerHandleError, PeerManager, SocketDescriptor,
};
use lightning::util::logger::Logger;

/// Spawns the threads necessary to manage a freshly accepted incoming
/// connection.
///
/// This function only needs to be called once for every incoming connection.
///
/// If the PeerManager accepts the connection, this function returns Ok with a
/// std::thread::JoinHandle<()> for the thread managing the connection in case
/// there is some need to `join` on it.
pub fn spawn_inbound_handler<CMH, RMH, L, UMH>(
    peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
    stream: TcpStream,
) -> Result<(), PeerHandleError>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    let ip_addr = stream.peer_addr().unwrap();
    let (conn, write_tx) = Connection::init(stream);
    let am_conn = Arc::new(Mutex::new(conn));
    let mut socket_descriptor = SyncSocketDescriptor::from_connection(am_conn, &write_tx);

    let net_address = match ip_addr.ip() {
        IpAddr::V4(ip) => NetAddress::IPv4 {
            addr: ip.octets(),
            port: ip_addr.port(),
        },
        IpAddr::V6(ip) => NetAddress::IPv6 {
            addr: ip.octets(),
            port: ip_addr.port(),
        },
    };

    // Notify the PeerManager of the new inbound connection.
    peer_manager
        .new_inbound_connection(socket_descriptor.clone(), Some(net_address))
        .map_err(|e| {
            // PeerManager rejected this connection, disconnect
            socket_descriptor.disconnect_socket();
            e
        })
}

/// An synchronous SocketDescriptor (i.e. it doesn't rely on Tokio)
#[derive(Clone)]
pub struct SyncSocketDescriptor {
    id: u64,
    am_conn: Arc<Mutex<Connection>>,
    write_tx: Sender<Vec<u8>>,
}
impl PartialEq for SyncSocketDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for SyncSocketDescriptor {}
impl hash::Hash for SyncSocketDescriptor {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}
impl SyncSocketDescriptor {
    fn from_connection(am_conn: Arc<Mutex<Connection>>, write_tx: &Sender<Vec<u8>>) -> Self {
        let id = am_conn.lock().unwrap().id;
        let write_tx = write_tx.clone();

        Self {
            id,
            am_conn,
            write_tx,
        }
    }
}
impl SocketDescriptor for SyncSocketDescriptor {
    fn send_data(&mut self, data: &[u8], _resume_read: bool) -> usize {
        if data.is_empty() {
            return 0;
        }

        // TODO: try_send() on write_tx

        unimplemented!();
    }

    fn disconnect_socket(&mut self) {
        self.am_conn.lock().unwrap().disconnect();
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Connection holds all the internal state for a connection.
struct Connection {
    id: u64,
    /// Whether reads are paused. See send_data() docs
    read_paused: bool,
}
impl Connection {
    /// Generates a new Connection given an existing TcpStream and spawns the
    /// processing threads for ConnectionReader and ConnectionWriter.
    ///
    /// Additionally returns a `write_tx` which can be used to pass data to the
    /// ConnectionWriter to send over TCP.
    fn init(original: TcpStream) -> (Self, Sender<Vec<u8>>) {
        let id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
        let clone = original.try_clone().expect("Clone failed");

        let tcp_reader = TcpReader(original);
        let tcp_writer = TcpWriter(clone);
        let reader = ConnectionReader::from_tcp_reader(tcp_reader);
        let (writer, write_tx) = ConnectionWriter::from_tcp_writer(tcp_writer);

        // Spawn the reader and writer threads
        thread::spawn(move || reader.run());
        thread::spawn(move || writer.run());

        let me = Self {
            id,
            read_paused: false,
        };

        (me, write_tx)
    }

    fn disconnect(&self) {
        unimplemented!();
    }
}

struct ConnectionReader {
    inner: TcpReader,
}
impl ConnectionReader {
    fn from_tcp_reader(reader: TcpReader) -> Self {
        Self { inner: reader }
    }

    fn run(&self) {
        unimplemented!()
    }
}

struct ConnectionWriter {
    inner: TcpWriter,
    write_rx: Receiver<Vec<u8>>,
}
impl ConnectionWriter {
    /// Generates a ConnectionWriter and associated `write_tx` from a TcpWriter
    fn from_tcp_writer(writer: TcpWriter) -> (Self, Sender<Vec<u8>>) {
        // Only one Vec<u8> can be in the channel at a time.
        // This way, senders can tell whether previous writes are still
        // processing by calling tx.is_full()
        let (write_tx, write_rx) = crossbeam_channel::bounded(1);
        let me = Self {
            inner: writer,
            write_rx,
        };

        (me, write_tx)
    }

    fn run(&self) {
        unimplemented!()
    }
}

/// A newtype for a TcpStream that can (and should) only be used for reading
struct TcpReader(TcpStream);
impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl TcpReader {
    /// Shuts down the read half of the underlying TcpStream.
    /// The write half needs to be shut down separately.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Read)
    }
}

/// A newtype for a TcpStream that can (and should) only be used for writing
struct TcpWriter(TcpStream);
impl Write for TcpWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
impl TcpWriter {
    /// Shuts down the write half of the underlying TcpStream.
    /// The read half needs to be shut down separately.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
}

#[cfg(test)]
mod tests {
    use super::Connection;
    use std::net::{TcpListener, TcpStream};

    fn create_connection() -> (TcpStream, TcpStream) {
        // We bind on localhost, hoping the environment is properly configured with a local
        // address. This may not always be the case in containers and the like, so if this test is
        // failing for you check that you have a loopback interface and it is configured with
        // 127.0.0.1.
        let (client, server) = if let Ok(server) = TcpListener::bind("127.0.0.1:9735") {
            (
                TcpStream::connect("127.0.0.1:9735").unwrap(),
                server.accept().unwrap().0,
            )
        } else if let Ok(server) = TcpListener::bind("127.0.0.1:9999") {
            (
                TcpStream::connect("127.0.0.1:9999").unwrap(),
                server.accept().unwrap().0,
            )
        } else if let Ok(server) = TcpListener::bind("127.0.0.1:46926") {
            (
                TcpStream::connect("127.0.0.1:46926").unwrap(),
                server.accept().unwrap().0,
            )
        } else {
            panic!("Failed to bind to v4 localhost on common ports");
        };

        (client, server)
    }

    #[test]
    fn basic_test() {
        let (client, _server) = create_connection();
        let _client_conn = Connection::init(client);

        // client_conn.reader.write();
        // client_conn.reader.0.write();
    }

    #[test]
    fn connect_to_stream() {
        assert!(true);
    }
}
