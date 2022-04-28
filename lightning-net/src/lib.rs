// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! A socket handling library for those using rust-lightning within a
//! synchronous, multi-threaded runtime, including inside SGX.
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
//! ## Overview of Channels in this crate
//!
//! - (`write_data_tx`, `write_data_rx`): A channel of `Vec<u8>`s with size 1
//!   from a [`SyncSocketDescriptor`] (multiple) to a [`Writer`] (singular) used
//!   for sending data to the [`Writer`] to send to the peer.
//! - TODO resume_read channel
//! - TODO complete
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

use crossbeam::channel::{Receiver, Sender};

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
    // Initialize all the channels. It has to be done here because otherwise there
    // is a circular dependency between the Connection and SyncSocketDescriptors

    // No reason to bound this channel
    let (resume_read_tx, resume_read_rx) = crossbeam::channel::unbounded();

    let ip_addr = stream.peer_addr().unwrap();
    let (conn, write_data_tx) = Connection::init(
        stream,
        peer_manager.clone(),
        resume_read_tx.clone(),
        resume_read_rx,
    );
    let am_conn = Arc::new(Mutex::new(conn));
    let conn_id = { am_conn.lock().unwrap().id };
    let mut socket_descriptor = SyncSocketDescriptor::new(conn_id, resume_read_tx, write_data_tx);

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
            // PeerManager rejected this connection; disconnect
            socket_descriptor.disconnect_socket();
            e
        })
}

// TODO implement a From trait for IpAddr -> NetAddress

/// An synchronous SocketDescriptor (i.e. it doesn't rely on Tokio)
/// TODO Describe what this SocketDescriptor *is* as well as what it *does*; to
/// a newcomer it's probably not immediately clear
#[derive(Clone)]
pub struct SyncSocketDescriptor {
    id: u64,
    resume_read_tx: Sender<()>,
    write_data_tx: Sender<Vec<u8>>,
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
    fn new(connection_id: u64, resume_read_tx: Sender<()>, write_data_tx: Sender<Vec<u8>>) -> Self {
        Self {
            id: connection_id,
            resume_read_tx,
            write_data_tx,
        }
    }
}
impl SocketDescriptor for SyncSocketDescriptor {
    /// TODO Write in a high level description of what this function (and more
    /// generally the SocketDescriptor) *does*
    ///
    /// This implementation never calls back into the PeerManager directly,
    /// thereby preventing reentrancy / deadlock issues. Instead, any commands
    /// to be processed and data to be sent are dispatched to the Reader or
    /// Writer via crossbeam channels.
    ///
    /// Additionally, sending across the crossbeam channels is done exclusively
    /// with non-blocking try_send()s rather than blocking send()s, to ensure
    /// that this function always returns immediately, thereby also reducing the
    /// amount of time that the PeerManager's internal locks are held.
    fn send_data(&mut self, data: &[u8], resume_read: bool) -> usize {
        if data.is_empty() {
            return 0;
        }

        if resume_read {
            // It doesn't really matter whether the send is Ok or Err:
            // - If Ok, the send went through, nothing else to do
            // - Since this channel is unbounded, an Err can only mean that the channel is
            //   disconnected. This might happen in the case that the Reader detected a
            //   disconnected peer and already shut itself down by the time this command was
            //   sent. There is no need to panic in this case
            let _ = self.resume_read_tx.try_send(());
        }

        // TODO Unpause reading when resume_read == true

        // TODO: try_send() on write_data_tx

        // The data must be copied here since a &[u8] reference cannot be sent
        // across threads, and a synchronous runtime requires dedicated threads
        // for reading and writing.
        // This copying
        // introduces a small amount of overhead. For a
        // zero-copy implementation, use `lightning-net-tokio`.

        unimplemented!();
    }

    fn disconnect_socket(&mut self) {
        // NOTE: Could send directly to reader / writer of this Connection
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Represents a TCP connection to a peer.
///
/// There should only be one `Connection` struct per TCP connection
/// (hence by init() takes ownership of the underlying TcpStream).
struct Connection {
    id: u64,
    /// Whether reads are paused. See send_data() docs
    read_paused: bool,
}
impl Connection {
    /// Generates a new Connection given an existing TcpStream and spawns
    /// processing threads for Reader and Writer.
    ///
    /// Reads and writes are blocking, so reading and writing is done on
    /// separate threads to prevent reads from blocking writes and vice versa.
    ///
    /// To achieve this, internally, the TcpStream is cloned and split into a
    /// TcpReader and TcpWriter. The TcpReader and TcpWriter newtypes are used
    /// to reinforce that they *should* only used for reading and writing
    /// respectively, but this is not enforced by the compiler due to their
    /// private tuple fields still being readable by the impls in this file.
    ///
    /// init() additionally returns a `write_data_tx` which can be used to pass
    /// data to the Writer to send over TCP.
    fn init<CMH, RMH, L, UMH>(
        original_stream: TcpStream,
        peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
        resume_read_tx: Sender<()>,
        resume_read_rx: Receiver<()>,
    ) -> (Self, Sender<Vec<u8>>)
    where
        CMH: ChannelMessageHandler + 'static + Send + Sync,
        RMH: RoutingMessageHandler + 'static + Send + Sync,
        L: Logger + 'static + ?Sized + Send + Sync,
        UMH: CustomMessageHandler + 'static + Send + Sync,
    {
        let id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
        let cloned_stream = original_stream.try_clone().expect("Clone failed");

        let (mut writer, write_data_tx) = Writer::new(TcpWriter(cloned_stream));
        let socket_descriptor =
            SyncSocketDescriptor::new(id, resume_read_tx, write_data_tx.clone());

        let mut reader: Reader<CMH, RMH, L, UMH> = Reader::new(
            TcpReader(original_stream),
            resume_read_rx,
            peer_manager,
            socket_descriptor,
        );

        // Spawn the reader and writer threads
        thread::spawn(move || reader.run());
        thread::spawn(move || writer.run());

        let me = Self {
            id,
            read_paused: false,
        };

        (me, write_data_tx)
    }

    fn disconnect(&self) {
        unimplemented!();
    }
}

// TODO Write doc description
struct Reader<CMH, RMH, L, UMH>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    inner: TcpReader,
    peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
    descriptor: SyncSocketDescriptor,
    resume_read_rx: Receiver<()>,
    read_paused: bool,
}
impl<CMH, RMH, L, UMH> Reader<CMH, RMH, L, UMH>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    fn new(
        reader: TcpReader,
        resume_read_rx: Receiver<()>,
        peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
        descriptor: SyncSocketDescriptor,
    ) -> Self {
        Self {
            inner: reader,
            peer_manager,
            resume_read_rx,
            descriptor,
            read_paused: false,
        }
    }

    // TODO write function description
    fn run(&mut self) {
        // 8KB is nice and big but also should never cause any issues with stack
        // overflowing.
        let mut buf = [0; 8192];

        loop {
            if self.read_paused {
                // To avoid a busy loop while reading is paused, block on the
                // resume_read channel until we are told to resume reading again
                // or until all channel senders are dropped (in which case we
                // will shut down).
                match self.resume_read_rx.recv() {
                    Ok(()) => {
                        // Resume reading
                        self.read_paused = false;
                    }
                    Err(_) => {
                        // The channel is disconnected, break and shut down
                        break;
                    }
                }
            } else {
                // Reading is not paused; block on the next read.
                // If the SyncSocketDescriptor disconnects the underlying TcpStream,
                // the Reader will read Ok(0) or Err, in which case we know to
                // break the loop and shut down.
                match self.inner.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        // Peer disconnected
                        break;
                    }
                    Ok(bytes_read) => {
                        // Register the read event with the PeerManager
                        match self
                            .peer_manager
                            .read_event(&mut self.descriptor, &buf[0..bytes_read])
                        {
                            Ok(pause_read) => {
                                if pause_read {
                                    self.read_paused = true;
                                }
                            }
                            Err(_) => {
                                // Rust-Lightning told us to disconnect; do it
                                // TODO
                                unimplemented!();
                            }
                        }

                        // TODO as noted in the docs for read_event(), we need
                        // to call process_events().
                    }
                }
            }
        }

        unimplemented!();
    }
}

// TODO Write doc description
struct Writer {
    inner: TcpWriter,
    write_data_rx: Receiver<Vec<u8>>,
    /// An internal buffer which stores the data that the Writer is
    /// currently attempting to write.
    ///
    /// This buffer is necessary because calls to self.inner.write() may fail or
    /// may write only part of the data.
    buf: Option<Vec<u8>>,
    /// The starting index into buf that specifies where in the buffer the next
    /// attempt should start.
    ///
    /// Partial writes are accounted for by incrementing the start index by the
    /// number of bytes written, while full writes reset `buf` back to None and
    /// the start index back to 0.
    ///
    /// Using this start index avoids the need to call buf.split_off() or
    /// .drain() which respectively incur the cost of an additional Vec
    /// allocation or data move.
    ///
    /// Writer code must maintain the invariant that
    /// `start < buf.len()`. If `start == buf.len()`, the value of `buf` should
    /// be `None`.
    start: usize,
}
impl Writer {
    /// Generates a Writer and associated `write_data_tx` from a
    /// TcpWriter
    fn new(stream: TcpWriter) -> (Self, Sender<Vec<u8>>) {
        // Only one Vec<u8> can be in the channel at a time. This way, senders
        // can know that previous writes are still processing when tx.is_full()
        // returns true.
        //
        // This channel between the Writer and the holders of the
        // Sender<Vec<u8>>s can be thought of as a second buffer, where the
        // first buffer is the Writer internal buffer (`self.buf`) and
        // the third buffer is the &[u8] passed into send_data().
        let (write_data_tx, write_data_rx) = crossbeam::channel::bounded(1);
        let me = Self {
            inner: stream,
            write_data_rx,
            buf: None,
            start: 0,
        };

        (me, write_data_tx)
    }

    // TODO: Write comment describing this function
    #[allow(clippy::single_match)]
    fn run(&mut self) {
        loop {
            // TODO: Handle WriterCommands

            match &self.buf {
                Some(buf) => {
                    // We have data in our internal buffer; attempt to write it
                    match self.inner.write(&buf[self.start..]) {
                        Ok(bytes_written) => {
                            // Define end s.t. the data written was buf[start..end]
                            let end = self.start + bytes_written;

                            if end == buf.len() {
                                // Everything was written, clear the buf and reset the start index
                                self.buf = None;
                                self.start = 0;
                            } else if bytes_written > 0 && end < buf.len() {
                                // Partial write; the new start index is exactly where the current
                                // write ended.
                                self.start = end;
                            } else if bytes_written == 0 {
                                // We received Ok, but nothing was written.
                                //
                                // Either the peer closed their read half only (unlikely) or the
                                // peer disconnected from us and we
                                // heard about it through our failed
                                // write (more likely). Break the loop so that we can send a
                                // disconnect.
                                break;
                            } else {
                                panic!("Unhandled case in Writer::run()");
                            }
                        }
                        Err(_) => {
                            // Write attempt errored; try again without updating
                            // the start index
                        }
                    }
                }
                None => {
                    // We don't have data in our internal buffer; fetch more
                    match self.write_data_rx.recv() {
                        Ok(data) => {
                            if !data.is_empty() {
                                // Data fetched, add it to the buffer
                                self.buf = Some(data);
                                self.start = 0;
                            }
                        }
                        Err(_) => {
                            // Channel is empty and disconnected
                            // => no more messages can be sent
                            // => break the loop and shut down the write half of the TcpStream
                            break;
                        }
                    }
                }
            }
        }

        // Wrapping up: shut down the TcpStream
        let _ = self.inner.shutdown();
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
    // use super::Connection;
    use std::net::{TcpListener, TcpStream};

    fn create_connection() -> (TcpStream, TcpStream) {
        // We bind on localhost, hoping the environment is properly configured with a
        // local address. This may not always be the case in containers and the
        // like, so if this test is failing for you check that you have a
        // loopback interface and it is configured with 127.0.0.1.
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
        let (_client, _server) = create_connection();
        // let _client_conn = Connection::init(client);

        // client_conn.reader.write();
        // client_conn.reader.0.write();
    }

    #[test]
    fn connect_to_stream() {
        assert!(true);
    }
}
