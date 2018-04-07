// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The daemon itself.

use daemonize;
use failure::{Error, ResultExt};
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use futures::future::Either;
use futures::sink::Send;
use futures::stream::{SplitSink, SplitStream, StreamFuture};
use futures::sync::{mpsc, oneshot};
use libc;
use state_machine_future::RentToOwn;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::marker::Send as StdSend;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use stund_protocol::*;
use tokio_core::reactor::{Core, Handle};
use tokio_io::AsyncRead;
use tokio_io::codec::length_delimited::{FramedRead, FramedWrite};
use tokio_io::codec::{BytesCodec, Framed};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_pty_process::{AsyncPtyMaster, Child, CommandExt};
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_signal;
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

        // Needed to command the creation of an SSH client

        let shared = Arc::new(Mutex::new(self));
        let shared3 = shared.clone();

        // The "main task" is just going to hang out monitoring a channel
        // waiting for someone to tell it to exit, because we might want to
        // exit for multiple reasons: we got a bad-news signal, or a client
        // told us to.

        let (tx_exit, rx_exit) = mpsc::channel(8);

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

        let handle2 = handle.clone();
        let tx_exit2 = tx_exit.clone();

        let server = listener.incoming().for_each(move |(socket, sockaddr)| {
            process_client(&handle2, socket, sockaddr, shared.clone(), tx_exit2.clone());
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


// Supporting jazz for managing SSH processes

type PtyStream = SplitStream<Framed<AsyncPtyMaster, BytesCodec>>;
type PtySink = SplitSink<Framed<AsyncPtyMaster, BytesCodec>>;

struct Tunnel {
    _tx_kill: oneshot::Sender<()>,
}


#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum ChildMonitor {
    #[state_machine_future(start, transitions(NotifyingChildDied))]
    AwaitingChildEvent {
        shared: Arc<Mutex<State>>,
        key: String,
        child: Child,
        rx_kill: oneshot::Receiver<()>,
        tx_die: mpsc::Sender<Option<ExitStatus>>, // None if child was explicitly killed
    },

    #[state_machine_future(transitions(ChildReaped))]
    NotifyingChildDied {
        tx_die: Send<mpsc::Sender<Option<ExitStatus>>>,
    },

    #[state_machine_future(ready)]
    ChildReaped(()),

    #[state_machine_future(error)]
    ChildError(()),
}

impl PollChildMonitor for ChildMonitor {
    fn poll_awaiting_child_event<'a>(
        state: &'a mut RentToOwn<'a, AwaitingChildEvent>
    ) -> Poll<AfterAwaitingChildEvent, ()> {
        match state.child.poll() {
            Err(_) => {
                return Err(());
            },

            Ok(Async::Ready(status)) => {
                // Child died! We no longer care about any kill messages, but
                // we should let the server know what happened.
                let mut state = state.take();
                //println!("child died womp womp");
                state.shared.lock().unwrap().children.remove(&state.key);
                state.rx_kill.close();
                transition!(NotifyingChildDied {
                    tx_die: state.tx_die.send(Some(status)),
                });
            },

            Ok(Async::NotReady) => {},
        }

        match state.rx_kill.poll() {
            Err(_) => {
                return Err(());
            },

            Ok(Async::Ready(_)) => {
                // We've been told to kill the child.
                let mut state = state.take();
                let _r = state.child.kill(); // can't do anything if this fails
                state.shared.lock().unwrap().children.remove(&state.key);
                state.rx_kill.close();
                transition!(NotifyingChildDied {
                    tx_die: state.tx_die.send(None),
                });
            },

            Ok(Async::NotReady) => {},
        }

        Ok(Async::NotReady)
    }

    fn poll_notifying_child_died<'a>(
        state: &'a mut RentToOwn<'a, NotifyingChildDied>
    ) -> Poll<AfterNotifyingChildDied, ()> {
        match state.tx_die.poll() {
            Err(_) => {
                return Err(());
            },

            Ok(Async::Ready(_)) => {
                transition!(ChildReaped(()));
            },

            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            },
        }
    }
}


// Oh right we actually want to handle clients too

fn process_client(
    handle: &Handle, socket: UnixStream, addr: SocketAddr, shared: Arc<Mutex<State>>,
    tx_exit: mpsc::Sender<()>,
) {
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
        handle: handle.clone(),
        shared: shared,
        _addr: addr,
        _tx_exit: Some(tx_exit),
    };

    let wrapped = Client::start(common, ser, de).map(move |(_common, _ser, _de)| {
        log!(shared2.lock().unwrap(), "client session finished");
    }).map_err(move |err| {
        log!(shared3.lock().unwrap(), "error from client session: {:?}", err);
    });

    handle.spawn(wrapped);
}


struct ClientCommonState {
    handle: Handle,
    shared: Arc<Mutex<State>>,
    _addr: SocketAddr,
    _tx_exit: Option<mpsc::Sender<()>>,
}

impl ClientCommonState {
    pub fn shared(&self) -> ::std::sync::MutexGuard<State> {
        self.shared.lock().unwrap()
    }
}

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum Client {
    #[state_machine_future(start, transitions(CommunicatingForOpen, Finished, Aborting))]
    AwaitingCommand {
        common: ClientCommonState,
        tx: Ser,
        rx: De,
    },

    #[state_machine_future(transitions(Aborting, CommunicatingForOpen, FinalizingOpen))]
    CommunicatingForOpen {
        common: ClientCommonState,
        cl_tx: Ser,
        cl_rx: De,
        cl_buf: Vec<u8>,
        ssh_tx: PtySink,
        ssh_rx: PtyStream,
        ssh_buf: Vec<u8>,
        ssh_die: StreamFuture<mpsc::Receiver<Option<ExitStatus>>>,
        saw_end: bool,
    },

    #[state_machine_future(transitions(AwaitingCommand))]
    FinalizingOpen {
        common: ClientCommonState,
        tx: Send<Ser>,
        rx: De,
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
        let msg = try_ready!(state.rx.poll());
        let state = state.take();

        match msg {
            None => {
                // Stream ended. (= connection closed?)
                transition!(Finished((state.common, state.tx, state.rx)));
            },

            Some(ClientMessage::Open(params)) => {
                return handle_open_command(state.common, params, state.tx, state.rx);
            },

            Some(ClientMessage::Exit) => {
                println!("XXX handle exit");
                transition!(Finished((state.common, state.tx, state.rx)));
            },

            Some(ClientMessage::Goodbye) => {
                transition!(Finished((state.common, state.tx, state.rx)));
            },

            Some(other) => {
                return Err(format_err!("unexpected message from client: {:?}", other));
            },
        }
    }

    /// The main thread has successfully started SSH! Now we do some
    /// uber-multiplexing to allow the client to communicate with the SSH
    /// process interactively, while keeping tabs on whether SSH bites the
    /// dust under us.
    fn poll_communicating_for_open<'a>(
        state: &'a mut RentToOwn<'a, CommunicatingForOpen>
    ) -> Poll<AfterCommunicatingForOpen, Error> {
        // New text from the user?

        while let Async::Ready(msg) = state.cl_rx.poll()? {
            match msg {
                Some(ClientMessage::UserData(data)) => {
                    if state.saw_end {
                        return Err(format_err!("client changed its mind about being finished"));
                    }

                    state.ssh_buf.extend_from_slice(&data);
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

                None => {
                    return Err(format_err!("client connection unexpectedly closed"));
                },
            }
        }

        // New text from SSH?

        loop {
            let outcome = match state.ssh_rx.poll() {
                Ok(x) => x,
                Err(e) => {
                    let msg = format!("something went wrong communicating with the SSH process: {}", e);
                    let mut state = state.take();
                    transition!(abort_client(state.common, state.cl_tx, state.cl_rx, msg));
                },
            };

            match outcome {
                Async::NotReady => break,

                Async::Ready(maybe_bytes) => {
                    if let Some(b) = maybe_bytes {
                        state.cl_buf.extend_from_slice(&b);
                    } else  {
                        // EOF from SSH -- it has probably died.
                        let msg = format!("unexpected EOF from SSH (program died?)");
                        let mut state = state.take();
                        transition!(abort_client(state.common, state.cl_tx, state.cl_rx, msg));
                    }
                }
            }
        }

        // Ready/able to send bytes to the client?

        if state.cl_buf.len() != 0 {
            let buf = state.cl_buf.clone();

            if let AsyncSink::Ready = state.cl_tx.start_send(ServerMessage::SshData(buf))? {
                state.cl_buf.clear();
            }
        }

        // Ready/able to send bytes to SSH?

        if state.ssh_buf.len() != 0 {
            let buf = state.ssh_buf.clone();

            if let AsyncSink::Ready = state.ssh_tx.start_send(buf.into())? {
                state.ssh_buf.clear();
            }
        }

        // Gotta flush those transmissions.

        try_ready!(state.cl_tx.poll_complete());
        try_ready!(state.ssh_tx.poll_complete());

        // What's next? Even if we're finished, we can't transition to the
        // next state until we're ready to send the OK message.

        if state.saw_end {
            let state = state.take();

            hand_off_ssh_process(&state.common.handle, state.common.shared.clone(),
                                 state.ssh_tx, state.ssh_rx);

            let send = state.cl_tx.send(ServerMessage::Ok);
            transition!(FinalizingOpen {
                common: state.common,
                tx: send,
                rx: state.cl_rx,
            });
        }

        Ok(Async::NotReady)
    }

    /// OMG, we actually started SSH successfully. Once we make sure that the
    /// client has received its success notification, we can go back to
    /// waiting for its next command.
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

    /// Something has happened that forces us to send the client an error
    /// message and terminate its connection. Make sure the message gets out.
    /// (Note that we must *not* return Err states in our state machine here
    /// because there is an important message that we must send to the client!
    /// The error was in the actions the client asked us to take, but not in
    /// the actual underlying handling of this connection.
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

fn handle_open_command(
    common: ClientCommonState, params: OpenParameters, mut tx: Ser, rx: De
) -> Poll<AfterAwaitingCommand, Error> {
    log!(common.shared(), "got command to spawn SSH for {}", params.host);

    let (tx_die, rx_die) = mpsc::channel(0);

    fn inner(
        common: &ClientCommonState, params: &OpenParameters, tx_die: mpsc::Sender<Option<ExitStatus>>
    ) -> Result<Framed<AsyncPtyMaster, BytesCodec>, Error> {
        let (tx_kill, rx_kill) = oneshot::channel();
        let ptymaster = AsyncPtyMaster::open(&common.handle).context("failed to create PTY")?;

        let child = process::Command::new("ssh")
            .arg(&params.host)
            .arg("echo \"[Login completed successfully.]\" && exec tail -f /dev/null")
            .env_remove("DISPLAY")
            .spawn_pty_async(&ptymaster, &common.handle).context("failed to launch SSH")?;

        // The task that will remember this child and wait around for it die.

        common.handle.spawn(ChildMonitor::start(
            common.shared.clone(), params.host.clone(), child, rx_kill, tx_die
        ));

        // The kill channel gives us a way to control the process later. We hold
        // on to the handles to the ptymaster and rx_die for now, because we care
        // about them when completing the password entry stage of the daemon
        // setup.

        common.shared().children.insert(params.host.clone(), Tunnel {
            _tx_kill: tx_kill,
        });

        Ok(ptymaster.framed(BytesCodec::new()))
    }

    match inner(&common, &params, tx_die) {
        Ok(ptymaster) => {
            let (ptywrite, ptyread) = ptymaster.split();

            if let Ok(AsyncSink::Ready) = tx.start_send(ServerMessage::Ok) {
            } else {
                panic!("cmon");
            }

            transition!(CommunicatingForOpen {
                common: common,
                cl_tx: tx,
                cl_rx: rx,
                cl_buf: Vec::new(),
                ssh_tx: ptywrite,
                ssh_rx: ptyread,
                ssh_buf: Vec::new(),
                ssh_die: rx_die.into_future(),
                saw_end: false,
            });
        },

        Err(e) => {
            let msg = format!("failed to launch SSH: {}", e);
            transition!(abort_client(common, tx, rx, msg));
        }
    }
}

// A task for monitoring each SSH process's PTY once it has successfully
// finished the password entry phase.

fn hand_off_ssh_process(
    handle: &Handle, shared: Arc<Mutex<State>>, _ssh_tx: PtySink, ssh_rx: PtyStream
) {
    //println!("handing off SSH process to monitor");
    let shared2 = shared.clone();

    let ssh_monitor = ssh_rx.for_each(move |bytes| {
        log!(shared.lock().unwrap(), "SSH: {:?}", bytes);
        Ok(())
    }).map_err(move |err| {
        log!(shared2.lock().unwrap(), "error polling SSH: {}", err);
    });

    handle.spawn(ssh_monitor);
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
