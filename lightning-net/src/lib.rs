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
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};

use lightning::ln::peer_handler::SocketDescriptor;

/// An synchronous SocketDescriptor (i.e. it doesn't rely on Tokio)
#[derive(Clone)]
struct SyncSocketDescriptor {
    id: u16,
    outbound_data: Arc<Mutex<Vec<Connection>>>,
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
impl SyncSocketDescriptor {
    fn new() -> Self {
        unimplemented!();
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

/// A TcpStream that can (and should) only be used for reading
struct TcpReader(TcpStream);
impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl TcpReader {
    /// Shuts down the read half of the underlying TcpStream.
    /// The write half needs to be shutdown separately.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Read)
    }
}

/// A TcpStream that can (and should) only be used for writing
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
    /// The read half needs to be shutdown separately.
    fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
}

/// Connection holds all the internal state for a connection.
struct Connection {
    reader: TcpReader,
    writer: TcpWriter,
}
impl Connection {
    /// Takes an existing TcpStream and splits it into a read half and write
    /// half. New types are used to prevent future implementations from
    /// accidentally calling read() from the writer thread or vice versa.
    fn from_std_stream(original: TcpStream) -> Self {
        let clone = original.try_clone().expect("Clone failed");
        Self {
            reader: TcpReader(original),
            writer: TcpWriter(clone),
        }
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
			(TcpStream::connect("127.0.0.1:9735").unwrap(), server.accept().unwrap().0)
		} else if let Ok(server) = TcpListener::bind("127.0.0.1:9999") {
			(TcpStream::connect("127.0.0.1:9999").unwrap(), server.accept().unwrap().0)
		} else if let Ok(server) = TcpListener::bind("127.0.0.1:46926") {
			(TcpStream::connect("127.0.0.1:46926").unwrap(), server.accept().unwrap().0)
		} else { panic!("Failed to bind to v4 localhost on common ports"); };

        (client, server)
    }

    #[test]
    fn basic_test() {
        let (client, _server) = create_connection();
        let _client_conn = Connection::from_std_stream(client);

        // client_conn.reader.write();
        // client_conn.reader.0.write();
    }

    #[test]
    fn connect_to_stream() {
        assert!(true);
    }
}
