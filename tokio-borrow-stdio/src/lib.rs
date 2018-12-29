// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

#![deny(missing_docs)]

//! "Borrow" the stdio streams temporarily, exposing them to a Tokio core as
//! nonblocking streams.
//!
//! As in many such implementations, the I/O is made nonblocking by spawning
//! threads to interact with the stdin and stdout streams. These exchange data
//! with the calling thread through [futures
//! channels](https://docs.rs/futures/*/futures/channel/index.html).

extern crate futures;
extern crate tokio_core;
extern crate tokio_io;

use futures::future::{Either, Loop};
use futures::sink::Send;
use futures::sync::{mpsc, oneshot};
use futures::{Async, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use std::io::{self, Error, ErrorKind, Read, Write};
use std::thread;
use tokio_core::reactor::Core;

type StdioPacket = Result<Vec<u8>, Error>;

struct StdinState<'a> {
    first: bool,
    buf: [u8; 512],
    stdin_lock: io::StdinLock<'a>,
    tx_stdin_data: Either<mpsc::Sender<StdioPacket>, Send<mpsc::Sender<StdioPacket>>>,
    rx_stdin_stop: oneshot::Receiver<()>,
}

/// "Borrow" the stdio streams to make them accessible to Tokio.
///
/// We make a best effort to clean up after the inner function is done, but
/// the stdin reader thread might stick around for the life of the process if
/// it is stuck blocking on a stdin read. It is therefore possible that a
/// chunk of stdin I/O will be lost.
pub fn borrow_stdio<F, T>(f: F) -> Result<T, Error>
where
    F: FnOnce(StdinStream, StdoutSink) -> Result<T, Error>,
{
    let (tx_stdin_data, rx_stdin_data) = mpsc::channel(1);
    let (tx_stdin_stop, rx_stdin_stop) = oneshot::channel::<()>();

    thread::spawn(move || {
        let mut core = Core::new().expect("couldn't create core");

        let stdin = io::stdin();
        let stdin_lock = stdin.lock();

        let state = StdinState {
            first: true,
            buf: [0u8; 512],
            stdin_lock: stdin_lock,
            tx_stdin_data: Either::A(tx_stdin_data),
            rx_stdin_stop: rx_stdin_stop,
        };

        let read_blocking = futures::future::loop_fn(state, |mut state| {
            // loop_fn calls the function immediately, which requires special
            // treatement since otherwise we'll panic since the core isn't yet
            // running.
            if state.first {
                state.first = false;
                return Ok(Loop::Continue(state));
            }

            if let Ok(Async::NotReady) = state.rx_stdin_stop.poll() {
            } else {
                return Ok(Loop::Break(()));
            }

            let tx_stdin_data = match state.tx_stdin_data {
                Either::A(tx) => tx,
                Either::B(mut send) => match send.poll() {
                    Ok(Async::NotReady) => {
                        state.tx_stdin_data = Either::B(send);
                        return Ok(Loop::Continue(state));
                    }
                    Ok(Async::Ready(tx)) => tx,
                    Err(e) => return Err(e),
                },
            };

            let msg = match state.stdin_lock.read(&mut state.buf[..]) {
                Ok(n) => Ok((&state.buf[..n]).to_owned()),
                Err(e) => Err(e),
            };

            state.tx_stdin_data = Either::B(tx_stdin_data.send(msg));
            Ok(Loop::Continue(state))
        });

        if let Err(e) = core.run(read_blocking) {
            eprintln!("error reading stdin in background thread: {}", e);
        }
    });

    let wrap_stdin = StdinStream(rx_stdin_data);

    // stdout

    let (tx_stdout_cmd, rx_stdout_cmd) = mpsc::channel::<Option<Vec<u8>>>(8);

    thread::spawn(move || {
        let stdout = io::stdout();
        let mut stdout_lock = stdout.lock();
        let rx = rx_stdout_cmd.wait();

        for item in rx {
            match item {
                Ok(None) => break,

                Ok(Some(data)) => {
                    let _r = stdout_lock.write(&data);
                    let _r = stdout_lock.flush();
                }

                Err(_) => {}
            };
        }
    });

    let wrap_stdout = StdoutSink(tx_stdout_cmd.clone());

    // Ready to go.

    let r = f(wrap_stdin, wrap_stdout);
    let _r = tx_stdin_stop.send(()); // can't react usefully if these fail
    let _r = tx_stdout_cmd.send(None);
    r
}

/// A futures-based stream of bytes from standard input.
#[derive(Debug)]
pub struct StdinStream(mpsc::Receiver<StdioPacket>);

impl Stream for StdinStream {
    type Item = Vec<u8>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Vec<u8>>, Error> {
        match self.0.poll() {
            Err(()) => Err(ErrorKind::Other.into()),

            Ok(Async::NotReady) => Ok(Async::NotReady),

            Ok(Async::Ready(None)) => Ok(Async::Ready(None)),

            Ok(Async::Ready(Some(io_res))) => match io_res {
                Ok(data) => {
                    if data.len() == 0 {
                        Ok(Async::Ready(None))
                    } else {
                        Ok(Async::Ready(Some(data)))
                    }
                }

                Err(e) => {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        Ok(Async::Ready(None))
                    } else {
                        Err(e)
                    }
                }
            },
        }
    }
}

/// A futures-based sink of bytes intended for standard output.
///
/// The inner storage is as an `Option` to make it so I can move values in and
/// out of the struct with a reference.
#[derive(Debug)]
pub struct StdoutSink(mpsc::Sender<Option<Vec<u8>>>);

impl Sink for StdoutSink {
    type SinkItem = Vec<u8>;
    type SinkError = Error;

    fn start_send(&mut self, item: Vec<u8>) -> StartSend<Vec<u8>, Error> {
        match self.0.start_send(Some(item)) {
            Err(_) => Err(io::ErrorKind::Other.into()),
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Ok(AsyncSink::NotReady(o)) => Ok(AsyncSink::NotReady(o.unwrap())),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Error> {
        match self.0.poll_complete() {
            Err(_) => Err(io::ErrorKind::Other.into()),
            Ok(x) => Ok(x),
        }
    }
}
