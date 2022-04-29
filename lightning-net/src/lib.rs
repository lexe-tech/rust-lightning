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
//! TODO rewrite the writer_cmd_tx description
//! - (`writer_cmd_tx`, `writer_cmd_rx`): A channel of `Vec<u8>`s with size 1
//!   from a [`SyncSocketDescriptor`] (multiple) to a [`Writer`] (singular) used
//!   for sending data to the [`Writer`] to send to the peer.
//! - TODO reader_cmd_tx, reader_cmd_rx channel
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
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam::channel::{Receiver, Sender, TryRecvError, TrySendError};

use lightning::ln::msgs::{ChannelMessageHandler, NetAddress, RoutingMessageHandler};
use lightning::ln::peer_handler::{
    CustomMessageHandler, PeerHandleError, PeerManager, SocketDescriptor,
};
use lightning::util::logger::Logger;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Spawns the threads necessary to manage a freshly accepted incoming
/// connection.
///
/// This function only needs to be called once for every incoming connection.
///
/// Returns the result of calling PeerManager::new_inbound_connection(),
pub fn setup_inbound<CMH, RMH, L, UMH>(
    peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
    stream: TcpStream,
) -> Result<(), PeerHandleError>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    // Init channels
    let (reader_cmd_tx, reader_cmd_rx, writer_cmd_tx, writer_cmd_rx) = init_channels();

    // Generate a new ID that represents this connection
    let conn_id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
    let socket_addr = stream.peer_addr().unwrap();

    // Init SyncSocketDescriptor
    let mut socket_descriptor =
        SyncSocketDescriptor::new(conn_id, reader_cmd_tx.clone(), writer_cmd_tx.clone());

    // Init TcpReader, TcpWriter, TcpDisconnectooor
    let writer_stream = stream.try_clone().unwrap();
    // let disconnector_stream = stream.try_clone().unwrap();
    let tcp_reader = TcpReader(stream);
    let tcp_writer = TcpWriter(writer_stream);
    // let tcp_disconnector = TcpDisconnectooor(disconnector_stream);

    // Init Reader and Writer
    let mut reader: Reader<CMH, RMH, L, UMH> = Reader::new(
        tcp_reader,
        peer_manager.clone(),
        reader_cmd_rx,
        writer_cmd_tx,
        socket_descriptor.clone(),
    );
    let mut writer: Writer<CMH, RMH, L, UMH> = Writer::new(
        tcp_writer,
        peer_manager.clone(),
        reader_cmd_tx,
        writer_cmd_rx,
    );

    // Spawn the reader and writer threads. Using a synchronous runtime
    // requires that there are dedicated threads for reading and writing.
    thread::spawn(move || reader.run());
    thread::spawn(move || writer.run());

    // Notify the PeerManager of the new inbound connection.
    peer_manager
        .new_inbound_connection(socket_descriptor.clone(), Some(to_net(socket_addr)))
        .map_err(|e| {
            // PeerManager rejected this connection; disconnect
            socket_descriptor.disconnect_socket();
            e
        })
}

// NOTE: It would be nice to have a `impl From<SocketAddr> for NetAddress` in
//       the `lightning` crate
fn to_net(socket_addr: SocketAddr) -> NetAddress {
    match socket_addr.ip() {
        IpAddr::V4(ip) => NetAddress::IPv4 {
            addr: ip.octets(),
            port: socket_addr.port(),
        },
        IpAddr::V6(ip) => NetAddress::IPv6 {
            addr: ip.octets(),
            port: socket_addr.port(),
        },
    }
}

/// Initializes all the crossbeam channels.
fn init_channels() -> (
    Sender<ReaderCommand>,
    Receiver<ReaderCommand>,
    Sender<WriterCommand>,
    Receiver<WriterCommand>,
) {
    // No reason to bound this channel
    let (reader_cmd_tx, reader_cmd_rx) = crossbeam::channel::unbounded();

    // The WriterCommand channel is size 2 so as to have 1 dedicated space for
    // each type of WriterCommand: WriteData and Shutdown. A Shutdown command
    // can be pushed into the channel at any time, but to ensure that there is
    // always space for it, send_data() will only ever push a WriteData command
    // into the channel after it first detects that the channel is completely
    // empty.
    //
    // The reason we can't have a second dedicated channel for sending Shutdown
    // commands to the Writer is that, being in a synchronous context, the
    // Writer can only block on one channel at once, which means that in the
    // case of two channels it could wait for WriteData commands or Shutdown
    // commands, but not both.
    //
    // Allocating only one slot in the channel for WriteData commands allows
    // send_data() to quickly detect that writes are still processing.
    // This space can be thought of as a second buffer, where the first buffer
    // is the Writer internal buffer (`self.buf`) and the third buffer is the
    // &[u8] passed into send_data().
    let (writer_cmd_tx, writer_cmd_rx) = crossbeam::channel::bounded(2);

    (reader_cmd_tx, reader_cmd_rx, writer_cmd_tx, writer_cmd_rx)
}

// TODO refactor setup_inbound into setup with an enum for inbound / outbound

// TODO implement a From trait for IpAddr -> NetAddress

/// An synchronous SocketDescriptor (i.e. it doesn't rely on Tokio)
/// TODO Describe what this SocketDescriptor *is* as well as what it *does*; to
/// a newcomer it's probably not immediately clear
#[derive(Clone)]
pub struct SyncSocketDescriptor {
    id: u64,
    reader_cmd_tx: Sender<ReaderCommand>,
    writer_cmd_tx: Sender<WriterCommand>,
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
    fn new(
        connection_id: u64,
        reader_cmd_tx: Sender<ReaderCommand>,
        writer_cmd_tx: Sender<WriterCommand>,
    ) -> Self {
        Self {
            id: connection_id,
            reader_cmd_tx,
            writer_cmd_tx,
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
            // It doesn't matter whether the send is Ok or Err
            let _ = self.reader_cmd_tx.try_send(ReaderCommand::ResumeRead);
        }

        // To ensure that there is always space for a Shutdown command, only
        // push data into the writer_cmd channel if it is currently empty.
        if self.writer_cmd_tx.is_empty() {
            // The data must be copied into the channel since a &[u8] reference
            // cannot be sent across threads. This incurs a small amount of overhead.
            let cmd = WriterCommand::WriteData(data.to_vec());
            match self.writer_cmd_tx.try_send(cmd) {
                Ok(()) => {
                    // Data was successfully sent to the Writer.
                    data.len()
                }
                Err(e) => match e {
                    TrySendError::Full(_) => {
                        // This could only happen if both channel slots were
                        // consumed in between the if check above and now - a
                        // TOCTTOU error. This shouldn't happen, but let's just
                        // proceed as normal: pause reads and return 0
                        let _ = self.reader_cmd_tx.try_send(ReaderCommand::PauseRead);
                        0
                    }
                    TrySendError::Disconnected(_) => {
                        // This might happen if the Writer detected a disconnect and
                        // shut down on its own. Return 0.
                        0
                    }
                },
            }
        } else {
            // There wasn't any space in the channel to hold the data. Pause.
            let _ = self.reader_cmd_tx.try_send(ReaderCommand::PauseRead);
            0
        }
    }

    // TODO shouldn't the PeerManager be notified if there has been a disconnect?

    /// There are several ways that a disconnect might be triggered:
    /// 1) The Reader receives Ok(0) or Err from TcpStream::read(), i.e. the
    ///    peer disconnected.
    /// 2) The Reader receives Err from PeerManager::read_event(), i.e.
    ///    Rust-Lightning told us to disconnect from the peer.
    /// 3) The Writer receives Ok(0) from TcpStream::write() (undocumented
    ///    behavior), or an Err(ErrorKind) that shouldn't be retried.
    /// 4) This function is called.
    ///
    /// The disconnect will be handled differently depending on the source of
    /// the trigger:
    /// - (1) and (2): If the Reader received the trigger, it will shut down
    ///   BOTH halves of the shared TcpStream AND send a Shutdown command to the
    ///   Reader.
    ///
    ///   - The explicit Shutdown command from the Reader is necessary because
    ///     if the Reader is blocked on `writer_cmd_rx.recv()` due to its
    ///     internal buffer being empty, the only way it can be unblocked is by
    ///     receiving a command, in this case the Shutdown command.
    ///   - The Reader closing both halves of the TCP stream is necessary
    ///     because while the writer is blocked on write(), the only way it can
    ///     unblock is by detecting the TCP disconnect.
    ///
    /// - (3): If the Writer received the trigger, it will shut down BOTH halves
    ///   of the shared TcpStream AND send a Shutdown command to the Reader.
    ///
    ///   - The explicit Shutdown command from the Writer is necessary because
    ///     if the Reader is blocked on `reader_cmd_rx.recv()` due to
    ///     `read_paused == true`, the only way it can be unblocked is by
    ///     receiving a command, in this case the Shutdown command.
    ///   - The Writer closing both halves of the TCP stream is necessary
    ///     because while the reader is blocked on read(), the only way it can
    ///     unblock is by detecting the TCP disconnect.
    ///
    /// - (4): If the disconnect was initiated here, a Shutdown command will be
    ///   sent to both the Reader and the Writer, AND the TcpDisconnectooor will
    ///   shutdown both halves of the shared TCP stream. The Shutdown command
    ///   ensures that the Reader / Writer will unblock if they are currently
    ///   blocked on `recv()`. The TCP stream shutdown ensures that they will
    ///   unblock if they are currently blocked on `read()` or `write()`
    ///   respectively.
    fn disconnect_socket(&mut self) {
        let _ = self.reader_cmd_tx.try_send(ReaderCommand::Shutdown);
        let _ = self.writer_cmd_tx.try_send(WriterCommand::Shutdown);
        // TODO implement the TcpDisconnectooor
    }
}

/// Commands that can be sent to the Reader.
enum ReaderCommand {
    ResumeRead,
    PauseRead,
    Shutdown, // TODO undo implementing this
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
    reader_cmd_rx: Receiver<ReaderCommand>,
    writer_cmd_tx: Sender<WriterCommand>,
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
        peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
        reader_cmd_rx: Receiver<ReaderCommand>,
        writer_cmd_tx: Sender<WriterCommand>,
        descriptor: SyncSocketDescriptor,
    ) -> Self {
        Self {
            inner: reader,
            peer_manager,
            reader_cmd_rx,
            writer_cmd_tx,
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
            // Every time this line is reached, read_paused == false.
            // Do a non-blocking try_recv() to see if we've been asked to pause reads
            let shutdown = self.handle_command(false);
            if shutdown {
                break;
            }

            if self.read_paused {
                // To avoid a busy loop while reading is paused, block on the
                // reader_cmd channel until we are told to resume reading again
                // or until we receive a shut down command.
                let shutdown = self.handle_command(true);
                if shutdown {
                    break;
                }
            } else {
                // Reading is not paused; block on the next read.
                // If the SyncSocketDescriptor disconnects the underlying TcpStream,
                // the Reader will read Ok(0), in which case we know to break
                // the loop and shut down.
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
                                break;
                            }
                        }

                        // As noted in the read_event() docs, call process_events().
                        self.peer_manager.process_events()
                    }
                }
            }
        }

        // Shut down the underlying stream. It's fine if it was already closed.
        let _ = self.inner.shutdown();
        // Send a signal to the Writer to do the same.
        let _ = self.writer_cmd_tx.try_send(WriterCommand::Shutdown);
    }

    /// Handles a potential ReaderCommand in a blocking or non-blocking manner.
    /// Returns a bool representing whether the Reader should break the event
    /// loop and shut down.
    fn handle_command(&mut self, block: bool) -> bool {
        if block {
            match self.reader_cmd_rx.recv() {
                Ok(cmd) => match cmd {
                    ReaderCommand::PauseRead => {
                        self.read_paused = true;
                    }
                    ReaderCommand::ResumeRead => {
                        self.read_paused = false;
                    }
                    ReaderCommand::Shutdown => return true,
                },
                Err(_) => {
                    // The channel is disconnected, break and shut down
                    return true;
                }
            }
        } else {
            match self.reader_cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    ReaderCommand::PauseRead => {
                        self.read_paused = true;
                    }
                    ReaderCommand::ResumeRead => {
                        self.read_paused = false;
                    }
                    ReaderCommand::Shutdown => return true,
                },
                Err(e) => match e {
                    TryRecvError::Empty => {}
                    TryRecvError::Disconnected => return true,
                },
            }
        }
        false
    }
}

/// Commands that can be sent to the Writer.
enum WriterCommand {
    WriteData(Vec<u8>),
    Shutdown,
}

// TODO Write doc description
struct Writer<CMH, RMH, L, UMH>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    inner: TcpWriter,
    peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
    reader_cmd_tx: Sender<ReaderCommand>,
    writer_cmd_rx: Receiver<WriterCommand>,
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
impl<CMH, RMH, L, UMH> Writer<CMH, RMH, L, UMH>
where
    CMH: ChannelMessageHandler + 'static + Send + Sync,
    RMH: RoutingMessageHandler + 'static + Send + Sync,
    L: Logger + 'static + ?Sized + Send + Sync,
    UMH: CustomMessageHandler + 'static + Send + Sync,
{
    /// Generates a Writer and associated `writer_cmd_tx` from a TcpWriter
    fn new(
        writer: TcpWriter,
        peer_manager: Arc<PeerManager<SyncSocketDescriptor, Arc<CMH>, Arc<RMH>, Arc<L>, Arc<UMH>>>,
        reader_cmd_tx: Sender<ReaderCommand>,
        writer_cmd_rx: Receiver<WriterCommand>,
    ) -> Self {
        Self {
            inner: writer,
            peer_manager,
            reader_cmd_tx,
            writer_cmd_rx,
            buf: None,
            start: 0,
        }
    }

    // TODO: Write comment describing this function
    #[allow(clippy::single_match)]
    fn run(&mut self) {
        use std::io::ErrorKind::*;

        loop {
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
                                // The behavior that produces this result is not
                                // clearly defined in the docs, but it's
                                // probably safe to assume that the correct
                                // response is to break the loop and shut down
                                // the TcpStream.
                                break;
                            } else {
                                panic!("Unhandled case in Writer::run()");
                            }
                        }
                        Err(e) => {
                            // Write attempt errored
                            match e.kind() {
                                TimedOut | Interrupted => {
                                    // Retry the write in the next loop
                                    // iteration if we received any of the above
                                    // errors. It would be nice to additionally
                                    // match HostUnreachable | NetworkDown |
                                    // ResourceBusy, but these require nightly
                                    // Rust.
                                }
                                _ => {
                                    // For all other errors, break and shut down
                                    break;
                                }
                            }
                        }
                    }
                }
                None => {
                    // We don't have data in our internal buffer; block on the
                    // command channel
                    match self.writer_cmd_rx.recv() {
                        Ok(cmd) => match cmd {
                            WriterCommand::WriteData(data) => {
                                if !data.is_empty() {
                                    // Data fetched, add it to the buffer
                                    self.buf = Some(data);
                                    self.start = 0;
                                }

                                // TODO call write_buffer_space_avail
                            }
                            WriterCommand::Shutdown => break,
                        },
                        Err(_) => {
                            // Channel is empty and disconnected
                            // => no more messages can be sent
                            // => break the loop and shut down
                            break;
                        }
                    }
                }
            }
        }

        // Shut down the underlying stream. It's fine if it was already closed.
        let _ = self.inner.shutdown();
        // Send a signal to the Reader to do the same.
        let _ = self.reader_cmd_tx.try_send(ReaderCommand::Shutdown);
    }
}

/// A newtype for a TcpStream that can (and should) only be used for reading.
/// Managed by the `Reader`.
struct TcpReader(TcpStream);
impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl TcpReader {
    /// Shuts down both halves of the underlying TcpStream.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Both)
    }
}

/// A newtype for a TcpStream that can (and should) only be used for writing.
/// Managed by the `Writer`.
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
    /// Shuts down both halves of the underlying TcpStream.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Both)
    }
}

/// A newtype for a TcpStream that can (and should) only be used for shutting
/// down the TcpStream. Managed by the `SyncSocketDescriptor`s.
struct TcpDisconnectooor(TcpStream);
// @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
// @@@@@@@@@@@@@@%%%%%%%%%%@@@@@@@@@@@@
// @@@@@@@@@@%###%@@@@@@@@%%##%@@@@@@@@
// @@@@@@@@#*%@@@@@%%%%%@@@@@@%##%@@@@@
// @@@@@@@##@@@@@@@@@@%%%@@@@@@@@#*@@@@
// @@@@@@%*@@@@@@@@@@@@@%%%%@%%@@@@*@@@
// @@@@@@*@@@@@@@@@%%%%%%@@@@@@@%%@**@@
// @@@@@@*@@@@@@@%#@@@@@%@%@@@@%%@@%*%@
// @@@@@%#@%%%%%@@@%##%%%##%@@%@@@@@#*@
// @@@@@%#*=%%##*#*-*+-:+#*=**#+==-*#:%
// @@@@@@*%%@@@@@@@=%#+=+%@-@@@:#-:@@:+
// @@@@@@@*@@%@#%@@#*#####*#@@#+##***=*
// @@@@@@@%*@%#:*@@@@@@@@@@@@@%##@@@#=#
// @@@@@@@@@*@@+=@@@@@@@@*#@%@@##@@@*=@
// @@@@@@@@@*@@%-=@@@@%#@%***#**%@@++@@
// @@@@@@@@@+@@@*-=@@@#%* ....: =%*=@@@
// @@@@@@@@##@@@%@=:#@@@*      .%*:%@@@
// @@@@@@@%+@@@@@@@*==#@@#. .:+#-=@@@@@
// @@@@@@#*@@@##%@@@@*=-+#*++**-*@@@@@@
// @%#####@@@#%@@@@@@@@%#+###**%%%%%#%%
// %%@@@@@@@@@@@@@@@%%%@%@@@@@@@@@@@@@@
// @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
impl Clone for TcpDisconnectooor {
    fn clone(&self) -> Self {
        Self(self.0.try_clone().unwrap())
    }
}
impl TcpDisconnectooor {
    /// Shuts down both halves of the underlying TcpStream.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Both)
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
