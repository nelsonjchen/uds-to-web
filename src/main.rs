//! A chat server that broadcasts a message to all connections.
//!
//! This is a simple line-based server which accepts WebSocket connections,
//! reads lines from those connections, and broadcasts the lines to all other
//! connected clients.
//!
//! You can test this out by running:
//!
//!     cargo run --example server 127.0.0.1:12345
//!
//! And then in another window run:
//!
//!     cargo run --example client ws://127.0.0.1:12345/
//!
//! You can run the second command in multiple windows and then chat between the
//! two, seeing the messages from the other client as they're received. For all
//! connected clients they'll all join the same room and see everyone else's
//! messages.

extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_tungstenite;
extern crate tokio_uds;
extern crate tungstenite;

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::io::{Error, ErrorKind};
use std::rc::Rc;

use bytes::BytesMut;
use futures::stream::Stream;
use futures::Future;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::codec::{Decoder, Encoder};
use tungstenite::protocol::Message;

use tokio_tungstenite::accept_async;

pub struct LineCodec;

impl Decoder for LineCodec {
    type Item = String;
    type Error = std::io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> std::io::Result<Option<String>> {
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            // remove the serialized frame from the buffer.
            let line = buf.split_to(i);

            // Also remove the '\n'
            buf.split_to(1);

            // Turn this data into a UTF string and return it in a Frame.
            match std::str::from_utf8(&line) {
                Ok(s) => Ok(Some(s.to_string())),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "invalid UTF-8",
                )),
            }
        } else {
            Ok(None)
        }
    }
}

impl Encoder for LineCodec {
    type Item = String;
    type Error = std::io::Error;

    fn encode(&mut self, msg: String, buf: &mut BytesMut) -> std::io::Result<()> {
        buf.extend(msg.as_bytes());
        buf.extend(b"\n");
        Ok(())
    }
}

fn main() {
    let addr = env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let addr = addr.parse().unwrap();

    // Create the event loop and TCP listener we'll accept connections on.
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let socket = TcpListener::bind(&addr, &handle).unwrap();
    println!("Listening on: {}", addr);

    // This is a single-threaded server, so we can just use Rc and RefCell to
    // store the map of all connections we know about.
    let connections = Rc::new(RefCell::new(HashMap::new()));

    let ws_srv = socket.incoming().for_each(|(stream, addr)| {
        // We have to clone both of these values, because the `and_then`
        // function billow constructs a new future, `and_then` requires
        // `FnOnce`, so we construct a move closure to move the
        // environment inside the future (AndThen future may overlive our
        // `for_each` future).
        let connections_inner = connections.clone();
        let handle_inner = handle.clone();

        accept_async(stream, None)
            .and_then(move |ws_stream| {
                println!("New WebSocket connection: {}", addr);

                // Create a channel for our stream, which other sockets will use to
                // send us messages. Then register our address with the stream to send
                // data to us.
                let (tx, rx) = futures::sync::mpsc::unbounded();
                connections_inner.borrow_mut().insert(addr, tx);

                // Let's split the WebSocket stream, so we can work with the
                // reading and writing halves separately.
                let (sink, stream) = ws_stream.split();

                // Whenever we receive a message from the client, we print it and
                // send to other clients, excluding the sender.
                let connections = connections_inner.clone();
                let ws_reader = stream.for_each(move |message: Message| {
                    println!("Received a message from {}: {}", addr, message);

                    // For each open connection except the sender, send the
                    // string via the channel.
                    let mut conns = connections.borrow_mut();
                    let iter = conns
                        .iter_mut()
                        .filter(|&(&k, _)| k != addr)
                        .map(|(_, v)| v);
                    for tx in iter {
                        tx.unbounded_send(message.clone()).unwrap();
                    }
                    Ok(())
                });

                // Whenever we receive a string on the Receiver, we write it to
                // `WriteHalf<WebSocketStream>`.
                let ws_writer = rx.fold(sink, |mut sink, msg| {
                    use futures::Sink;
                    sink.start_send(msg).unwrap();
                    Ok(sink)
                });

                // Now that we've got futures representing each half of the socket, we
                // use the `select` combinator to wait for either half to be done to
                // tear down the other. Then we spawn off the result.
                let connection = ws_reader
                    .map(|_| ())
                    .map_err(|_| ())
                    .select(ws_writer.map(|_| ()).map_err(|_| ()));

                handle_inner.spawn(connection.then(move |_| {
                    connections_inner.borrow_mut().remove(&addr);
                    println!("Connection {} closed.", addr);
                    Ok(())
                }));

                Ok(())
            })
            .map_err(|e| {
                println!("Error during the websocket handshake occurred: {}", e);
                Error::new(ErrorKind::Other, e)
            })
    });



    let uds = tokio_uds::UnixListener::bind("/tmp/uds", &handle).unwrap();
    let uds_srv = uds.incoming().for_each(|(stream, addr)| {
        // We have to clone both of these values, because the `and_then`
        // function billow constructs a new future, `and_then` requires
        // `FnOnce`, so we construct a move closure to move the
        // environment inside the future (AndThen future may overlive our
        // `for_each` future).
        let connections_inner = connections.clone();
        let handle_inner = handle.clone();
        let addr_inner = addr.clone();

        use tokio_io::AsyncRead;
        let (sink, stream) = stream.framed(LineCodec).split();

        let uds_reader = stream.for_each(move |line: String| {
            println!("Received a UDS message: {}", line);

            // For each open connection except the sender, send the
            // string via the channel.
            let mut conns = connections_inner.borrow_mut();
            let iter = conns.iter_mut().map(|(_, v)| v);
            for tx in iter {
                tx.unbounded_send(tungstenite::Message::Text(line.clone())).unwrap();
            }
            Ok(())
        });

        let connection = uds_reader
                    .map(|_| ())
                    .map_err(|_| ());

        handle_inner.spawn(connection);

        Ok(())
    });

    let mixed_srv = uds_srv.join(ws_srv);

    // Execute server.
    core.run(mixed_srv).unwrap();
}
