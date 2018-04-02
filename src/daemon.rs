// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The daemon itself.

use daemonize;
use failure::{Error, ResultExt};
use futures::future::Either;
use futures::sink::Send;
use futures::stream::StreamFuture;
use futures::sync::mpsc::{channel, Receiver};
use libc;
use pseudotty::{self, CommandPtyExt};
use state_machine_future::RentToOwn;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::marker::Send as StdSend;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use stund::*;
use stund::protocol::*;
use tokio::prelude::*;
use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited::{FramedRead, FramedWrite};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_signal;
use tokio_stdin;
use tokio_uds::{UnixListener, UnixStream};

use super::*;

type Ser = WriteJson<FramedWrite<WriteHalf<UnixStream>>, ServerMessage>;
type De = ReadJson<FramedRead<ReadHalf<UnixStream>>, ClientMessage>;


const FATAL_SIGNALS: &[i32] = &[
    libc::SIGABRT,
    libc::SIGBUS,
    libc::SIGFPE,
    libc::SIGHUP,
    libc::SIGILL,
    libc::SIGINT,
    libc::SIGKILL,
    libc::SIGPIPE,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGTRAP,
];


pub struct State {
    sock_path: PathBuf,
    _opts: StundDaemonOptions,
    log: Box<Write + StdSend>,
    children: HashMap<String, Tunnel>,
}

macro_rules! log {
    ($state:expr, $fmt:expr) => { $state.log_items(format_args!($fmt)) };
    ($state:expr, $fmt:expr, $($args:tt)*) => { $state.log_items(format_args!($fmt, $($args)*)) };
}

impl State {
    pub fn new(opts: StundDaemonOptions) -> Result<Self, Error> {
        let p = get_socket_path()?;

        if StdUnixStream::connect(&p).is_ok() {
            return Err(format_err!("refusing to start: another daemon is already running"));
        }

        match fs::remove_file(&p) {
            Ok(_) => {},
            Err(e) => {
                match e.kind() {
                    io::ErrorKind::NotFound => {},
                    _ => {
                        return Err(e.into());
                    },
                }
            },
        }

        // Make sure our socket and logs will be only accessible to us!
        unsafe { libc::umask(0o177); }

        let log: Box<Write + StdSend> = if opts.foreground {
            println!("stund daemon: staying in foreground");
            Box::new(io::stdout())
        } else {
            let mut log_path = p.clone();
            log_path.set_extension("log");

            let log = fs::File::create(&log_path)?;
            daemonize::Daemonize::new().start()?;
            Box::new(log)
        };

        Ok(State {
            sock_path: p,
            _opts: opts,
            log: log,
            children: HashMap::new(),
        })
    }


    /// Don't use this directly; use the log!() macro.
    fn log_items(&mut self, args: fmt::Arguments) {
        let _r = writeln!(self.log, "{}", args);
        let _r = self.log.flush();
    }


    pub fn serve(mut self) -> Result<(), Error> {
        let mut core = Core::new()?;
        let handle = core.handle();
        let listener = UnixListener::bind(&self.sock_path, &handle)?;

        log!(self, "starting up");
        let shared = Arc::new(Mutex::new(self));
        let shared3 = shared.clone();

        // The "main task" is just going to hang out monitoring a channel
        // waiting for someone to tell it to exit, because we might want to
        // exit for multiple reasons: we got a bad-news signal, or a client
        // told us to.

        let (tx_exit, rx_exit) = channel(8);

        // signal handling -- we're forced to have one stream for each signal;
        // we spawn a task for each of these that forwards an exit
        // notification to the main task. Shenanigans here because
        // `.and_then()` requires compatible error types before and after, and
        // `spawn()` requires both the item and error types to be (). Finally,
        // we have to clone `tx_exit2` *in the closure* because the `send()`
        // call consumes the value, which has been captured, and the type
        // system doesn't/can't know that the closure will only ever be called
        // once.

        for signal in FATAL_SIGNALS {
            let sig_stream = tokio_signal::unix::Signal::new(*signal, &handle).flatten_stream();
            let shared2 = shared.clone();
            let tx_exit2 = tx_exit.clone();

            let stream = sig_stream
                .map_err(|_| {})
                .and_then(move |sig| {
                    log!(shared2.lock().unwrap(), "exiting on signal {}", sig);
                    tx_exit2.clone().send(()).map_err(|_| {})
                });

            let fut = stream.into_future().map(|_| {}).map_err(|_| {});

            handle.spawn(fut);
        }

        // handling incoming connections -- normally this is the "main" task
        // of a server, but we have all sorts of cares and worries.

        //let tx_exit2 = tx_exit.clone();

        let server = listener.incoming().for_each(move |(socket, sockaddr)| {
            //process_client(socket, sockaddr, shared.clone(), tx_exit2.clone());
            process_client(socket, sockaddr, shared.clone());
            Ok(())
        }).map_err(move |err| {
            log!(shared3.lock().unwrap(), "accept error: {:?}", err);
        });

        handle.spawn(server);

        // The return and error values of the wait-to-die task are
        // meaningless.

        let _r = core.run(rx_exit.into_future());
        Ok(())
    }
}


fn process_client(socket: UnixStream, addr: SocketAddr, shared: Arc<Mutex<State>>) {
    // Without turning on linger, I find that the tokio-ized version loses
    // the last bytes of the session. Let's just ignore the return value
    // of setsockopt(), though.

    unsafe {
        let linger = libc::linger { l_onoff: 1, l_linger: 2 };
        libc::setsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
                         (&linger as *const libc::linger) as _,
                         mem::size_of::<libc::linger>() as libc::socklen_t);
    }

    let (read, write) = socket.split();
    let wdelim = FramedWrite::new(write);
    let ser = WriteJson::new(wdelim);
    let rdelim = FramedRead::new(read);
    let de = ReadJson::new(rdelim);

    let shared2 = shared.clone();
    let shared3 = shared.clone();

    let common = ClientCommonState {
        shared: shared,
        addr: addr,
    };

    let wrapped = Client::start(common, ser, de.into_future()).map(move |(_common, _ser, _de)| {
        log!(shared2.lock().unwrap(), "client session finished");
    }).map_err(move |err| {
        log!(shared3.lock().unwrap(), "error from client session: {:?}", err);
    });

    tokio::spawn(wrapped);
}


struct ClientCommonState {
    shared: Arc<Mutex<State>>,
    addr: SocketAddr,
}

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum Client {
    #[state_machine_future(start, transitions(AwaitingCommand, CommunicatingForOpen, Finished, Aborting))]
    AwaitingCommand {
        common: ClientCommonState,
        tx: Ser,
        rx: StreamFuture<De>,
    },

    #[state_machine_future(transitions(Aborting, CommunicatingForOpen, FinalizingOpen))]
    CommunicatingForOpen {
        common: ClientCommonState,
        cl_tx: Either<Ser, Send<Ser>>,
        cl_rx: StreamFuture<De>,
        ssh_rx: StreamFuture<Receiver<u8>>,
        ptymaster: fs::File,
        buf: Vec<u8>,
        saw_end: bool,
    },

    #[state_machine_future(transitions(AwaitingCommand))]
    FinalizingOpen {
        common: ClientCommonState,
        tx: Send<Ser>,
        rx: StreamFuture<De>,
    },

    #[state_machine_future(ready)]
    Finished((ClientCommonState, Ser, De)),

    #[state_machine_future(transitions(Aborting, Failed))]
    Aborting {
        common: ClientCommonState,
        tx: Send<Ser>,
        rx: Either<De, StreamFuture<De>>,
        message: Option<String>,
    },

    #[state_machine_future(error)]
    Failed(Error),
}


impl PollClient for Client {
    fn poll_awaiting_command<'a>(
        state: &'a mut RentToOwn<'a, AwaitingCommand>
    ) -> Poll<AfterAwaitingCommand, Error> {
        let (msg, de) = match state.rx.poll() {
            Ok(Async::Ready((msg, de))) => (msg, de),
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            },
            Err((e, _de)) => {
                return Err(e.into());
            }
        };

        let mut state = state.take();

        match msg {
            None => {
                state.rx = de.into_future();
                transition!(state);
            },

            Some(ClientMessage::Open(params)) => {
                return handle_client_open(state.common, state.tx, de, params);
            },

            Some(ClientMessage::Exit) => {
                println!("XXX handle exit");
                transition!(Finished((state.common, state.tx, de)));
            },

            Some(ClientMessage::Goodbye) => {
                transition!(Finished((state.common, state.tx, de)));
            },

            Some(other) => {
                return Err(format_err!("unexpected message from client: {:?}", other));
            },
        }
    }

    fn poll_communicating_for_open<'a>(
        state: &'a mut RentToOwn<'a, CommunicatingForOpen>
    ) -> Poll<AfterCommunicatingForOpen, Error> {
        // New text from the user?

        let de = {
            let outcome = match state.cl_rx.poll() {
                Ok(x) => x,
                Err((e, _de)) => {
                    return Err(e.into());
                },
            };

            if let Async::Ready((msg, de)) = outcome {
                match msg {
                    Some(ClientMessage::UserData(data)) => {
                        if state.saw_end {
                            return Err(format_err!("client changed its mind about being finished"));
                        }

                        if let Err(e) = state.ptymaster.write_all(&data) {
                            let msg = format!("error writing to SSH process: {}", e);
                            let mut state = state.take();
                            transition!(abort_client(state.common, state.cl_tx, de, msg));
                        }
                    },

                    Some(ClientMessage::EndOfUserData) => {
                        state.saw_end = true;
                    },

                    Some(other) => {
                        // Could consider aborting here, but if we didn't
                        // understand the client then probably there's
                        // something messed up about the channel.
                        return Err(format_err!("unexpected message from the client: {:?}", other));
                    },

                    None => {},
                }

                Some(de)
            } else {
                None
            }
        };

        // New text from SSH?

        let rcvr = {
            let outcome = match state.ssh_rx.poll() {
                Ok(x) => x,
                Err((_, _stdin)) => {
                    let msg = format!("something went wrong communicating with the SSH process");
                    let mut state = state.take();
                    let rx = if let Some(updated) = de {
                        updated.into_either_rx()
                    } else {
                        state.cl_rx.into_either_rx()
                    };
                    transition!(abort_client(state.common, state.cl_tx, rx, msg));
                },
            };

            if let Async::Ready((byte, stdin)) = outcome {
                if let Some(b) = byte {
                    state.buf.push(b);
                }

                Some(stdin)
            } else {
                None
            }
        };

        // Ready/able to send bytes to the client?

        let mut state = state.take();

        let cl_tx = match state.cl_tx {
            Either::A(ser) => {
                if state.buf.len() != 0 {
                    let send = ser.send(ServerMessage::SshData(state.buf.clone()));
                    state.buf.clear();
                    Either::B(send)
                } else {
                    Either::A(ser)
                }
            },

            Either::B(mut send) => {
                Either::A(try_ready!(send.poll()))
            },
        };

        // What's next? Even if we're finished, we can't transition to the
        // next state until we're ready to send the OK message.

        if let Some(rcvr) = rcvr {
            state.ssh_rx = rcvr.into_future();
        }

        if let Some(de) = de {
            state.cl_rx = de.into_future();
        }

        if state.saw_end {
            if let Either::A(ser) = cl_tx {
                // XXX: stash handle to SSH pty

                let send = ser.send(ServerMessage::Ok);
                transition!(FinalizingOpen {
                    common: state.common,
                    tx: send,
                    rx: state.cl_rx,
                });
            }
        }

        state.cl_tx = cl_tx;
        transition!(state);
    }

    fn poll_finalizing_open<'a>(
        state: &'a mut RentToOwn<'a, FinalizingOpen>
    ) -> Poll<AfterFinalizingOpen, Error> {
        let mut state = state.take();
        let ser = try_ready!(state.tx.poll());

        transition!(AwaitingCommand {
            common: state.common,
            tx: ser,
            rx: state.rx,
        });
    }

    fn poll_aborting<'a>(
        state: &'a mut RentToOwn<'a, Aborting>
    ) -> Poll<AfterAborting, Error> {
        let ser = try_ready!(state.tx.poll());
        let mut state = state.take();

        if let Some(msg) = state.message {
            state.tx = ser.send(ServerMessage::Error(msg));
            state.message = None;
            transition!(state)
        } else {
            Err(format_err!("ending connection now that client has been notified"))
        }
    }
}


// Little framework for being able to transition into an "abort" state, where
// we notify the client of an error and then close the connection. The tricky
// part is that we'd like this to work regardless of whether we're in `Ser`
// state or `Send<Ser>` state. In the latter, we need to wait for the previous
// send to complete before we can send the error message. Ditto for the
// reception side, although we do not plan to listen for any more data on this
// connection.

trait IntoEitherTx { fn into_either_tx(self) -> Either<Ser, Send<Ser>>; }

impl IntoEitherTx for Ser {
    fn into_either_tx(self) -> Either<Ser, Send<Ser>> { Either::A(self) }
}

impl IntoEitherTx for Send<Ser> {
    fn into_either_tx(self) -> Either<Ser, Send<Ser>> { Either::B(self) }
}

impl IntoEitherTx for Either<Ser, Send<Ser>> {
    fn into_either_tx(self) -> Either<Ser, Send<Ser>> { self }
}

trait IntoEitherRx { fn into_either_rx(self) -> Either<De, StreamFuture<De>>; }

impl IntoEitherRx for De {
    fn into_either_rx(self) -> Either<De, StreamFuture<De>> { Either::A(self) }
}

impl IntoEitherRx for StreamFuture<De> {
    fn into_either_rx(self) -> Either<De, StreamFuture<De>> { Either::B(self) }
}

impl IntoEitherRx for Either<De, StreamFuture<De>> {
    fn into_either_rx(self) -> Either<De, StreamFuture<De>> { self }
}

fn abort_client<T: IntoEitherTx, R: IntoEitherRx>(
    common: ClientCommonState, tx: T, rx: R, message: String
) -> Aborting {
    let tx = tx.into_either_tx();
    let rx = rx.into_either_rx();

    let (tx, msg) = match tx {
        Either::A(ser) => {
            (ser.send(ServerMessage::Error(message)), None)
        },

        Either::B(snd) => {
            (snd, Some(message))
        },
    };

    Aborting {
        common: common,
        tx: tx,
        rx: rx,
        message: msg,
    }
}


fn handle_client_open(
    common: ClientCommonState, tx: Ser, rx: De, params: OpenParameters
) -> Poll<AfterAwaitingCommand, Error> {
    let result = {
        let mut shared = common.shared.lock().unwrap();
        handle_client_open_inner(&mut shared, &params)
    };

    let (ptymaster1, ptymaster2) = match result {
        Ok(m) => m,

        Err(e) => { // We have to tell the client that something went wrong.
            transition!(abort_client(common, tx, rx, format!("{}", e)));
        }
    };

    let tx = tx.send(ServerMessage::Ok);
    let ssh_rx = spawn_monitor(ptymaster1, 512);

    transition!(CommunicatingForOpen {
        common: common,
        cl_tx: Either::B(tx),
        cl_rx: rx.into_future(),
        ssh_rx: ssh_rx.into_future(),
        ptymaster: ptymaster2,
        buf: Vec::new(),
        saw_end: false,
    });
}

fn handle_client_open_inner(shared: &mut State, params: &OpenParameters) -> Result<(fs::File, fs::File), Error> {
    // First, the PTY.

    let ptymaster = pseudotty::create().context("failed to create PTY")?;
    let ptymaster_dup = ptymaster.try_clone().context("failed to clone PTY handle")?;
    let ptymaster_dup2 = ptymaster.try_clone().context("failed to clone PTY handle")?;

    let child = process::Command::new("ssh")
        .arg("-N")
        .arg(&params.host)
        .env_remove("DISPLAY")
        .slave_to_pty(&ptymaster).context("failed to set up PTY slave")?
        .spawn().context("failed to launch SSH command")?;

    // To reap children properly, we need to log them here -- note that this
    // is before we know if we've actually set up the SSH session fully.

    shared.children.insert(params.host.clone(), Tunnel {
        ssh: child,
        ptymaster: ptymaster_dup,
    });

    Ok((ptymaster, ptymaster_dup2))
}



struct Tunnel {
    ssh: process::Child,

    /// Needed to maintain the SSH daemon's PTY! Otherwise it SIGHUPs and dies
    /// if it tries to do any output.
    #[allow(unused)] ptymaster: fs::File,
}


/// This just duplicates what tokio-stdin does.
pub fn spawn_monitor<R: 'static + Read + StdSend>(thing: R, buffer: usize) -> Receiver<u8> {
    // TODO: we need to know when to quit! Or maybe we actually don't.
    use futures::stream::iter_result;
    use futures::sync::mpsc::{channel};
    use std::thread;

    let (channel_sink, channel_stream) = channel(buffer);
    let reader_sink = channel_sink.sink_map_err(|e| Error::from(e));

    thread::spawn(move || {
        let _r = iter_result(thing.bytes())
            .map_err(|e| Error::from(e))
            .forward(reader_sink)
            .wait();
    });

    channel_stream
}
