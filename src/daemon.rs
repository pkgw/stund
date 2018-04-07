// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The daemon itself.

use daemonize;
use failure::{Error, ResultExt};
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use futures::future::Either;
use futures::sink::Send;
use futures::stream::{SplitSink, SplitStream, StreamFuture};
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::sync::oneshot;
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
    tx_new_ssh: Option<Sender<(OpenParameters, TxFin)>>,
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
            tx_new_ssh: None,
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

        let (tx_new_ssh, rx_new_ssh) = channel(8);
        self.tx_new_ssh = Some(tx_new_ssh);

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

        // As far as I can tell, in Tokio 0.1 we can only create new evented I/O thingies
        // from this main thread, which means yet another custom task that listens for
        // instructions to start new SSH processes.

        let handle2 = handle.clone();
        let shared2 = shared.clone();

        let ssh_maker = rx_new_ssh.for_each(move |(params, tx_finished): (OpenParameters, TxFin)| {
            // A channel that the server can use to tell the SSH monitor task to kill the
            // process, and a channel that the monitor can use to tell us if SSH died.

            //println!("got command to spawn SSH for {}", params.host);

            let (tx_kill, rx_kill) = oneshot::channel();
            let (tx_die, rx_die) = channel(0);

            let inner = || {
                let ptymaster = AsyncPtyMaster::open(&handle2).context("failed to create PTY")?;

                // Now actually launch the SSH process.

                let child = process::Command::new("ssh")
                    .arg(&params.host)
                    .arg("echo \"[Login completed successfully.]\" && tail -f /dev/null")
                    .env_remove("DISPLAY")
                    .spawn_pty_async(&ptymaster, &handle2).context("failed to launch SSH")?;

                // The task that will remember this child and wait around for it die.

                handle2.spawn(ChildMonitor::start(
                    shared2.clone(), params.host.clone(), child, rx_kill, tx_die
                ));

                // The kill channel gives us a way to control the process later. We hold
                // on to the handles to the ptymaster and rx_die for now, because we care
                // about them when completing the password entry stage of the daemon
                // setup.

                shared2.lock().unwrap().children.insert(params.host.clone(), Tunnel {
                    _tx_kill: tx_kill,
                });

                let (ptywrite, ptyread) = ptymaster.framed(BytesCodec::new()).split();
                Ok(NewSshInfo { ptywrite: ptywrite, ptyread: ptyread, rx_die: rx_die })
            };

            let _r = tx_finished.send(inner()); // can't use this under current model
            Ok(())
        });

        handle.spawn(ssh_maker);

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


// Supporting jazz for the SSH-creation exchange

struct NewSshInfo {
    pub ptywrite: PtySink,
    pub ptyread: PtyStream,
    pub rx_die: Receiver<Option<ExitStatus>>,
}


type PtyStream = SplitStream<Framed<AsyncPtyMaster, BytesCodec>>;
type PtySink = SplitSink<Framed<AsyncPtyMaster, BytesCodec>>;

type TxFin = oneshot::Sender<Result<NewSshInfo, Error>>;

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
        tx_die: Sender<Option<ExitStatus>>, // None if child was explicitly killed
    },

    #[state_machine_future(transitions(ChildReaped))]
    NotifyingChildDied {
        tx_die: Send<Sender<Option<ExitStatus>>>,
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
    tx_exit: Sender<()>,
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
    _tx_exit: Option<Sender<()>>,
}

impl ClientCommonState {
    pub fn shared(&self) -> ::std::sync::MutexGuard<State> {
        self.shared.lock().unwrap()
    }
}

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum Client {
    #[state_machine_future(start, transitions(AwaitingCommand, StartSendSshSpawn, Finished, Aborting))]
    AwaitingCommand {
        common: ClientCommonState,
        tx: Ser,
        rx: De,
    },

    #[state_machine_future(transitions(PollSendSshSpawn))]
    StartSendSshSpawn {
        common: ClientCommonState,
        params: OpenParameters,
        cl_tx: Ser,
        cl_rx: De,
    },

    #[state_machine_future(transitions(WaitingForSshSpawn))]
    PollSendSshSpawn {
        common: ClientCommonState,
        rx_ssh_ready: oneshot::Receiver<Result<NewSshInfo, Error>>,
        cl_tx: Ser,
        cl_rx: De,
    },

    #[state_machine_future(transitions(Aborting, CommunicatingForOpen))]
    WaitingForSshSpawn {
        common: ClientCommonState,
        rx_ssh_ready: oneshot::Receiver<Result<NewSshInfo, Error>>,
        cl_tx: Ser,
        cl_rx: De,
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
        ssh_die: StreamFuture<Receiver<Option<ExitStatus>>>,
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
                // In tokio 0.1, we have to do a dance because only the main
                // thread can launch a new evented I/O thingie. First, start
                // sending a message to the main thread telling it to launch
                // an SSH process.
                transition!(StartSendSshSpawn {
                    common: state.common,
                    params: params,
                    cl_tx: state.tx,
                    cl_rx: state.rx,
                });
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

    /// Try to *start* sending a message to the main thread telling it to
    /// start a new SSH process.
    ///
    /// (We have to use `start_send()` rather then `send()` since the latter
    /// consumes the sink, which we can't do since it is a shared resource.)
    fn poll_start_send_ssh_spawn<'a>(
        state: &'a mut RentToOwn<'a, StartSendSshSpawn>
    ) -> Poll<AfterStartSendSshSpawn, Error> {
        //println!("start send ssh spawn");
        let (tx_ssh_ready, rx_ssh_ready) = oneshot::channel();
        let mut state = state.take();
        let r = { state.common.shared().tx_new_ssh.as_mut().unwrap().start_send((state.params, tx_ssh_ready))? };
        match r {
            futures::AsyncSink::Ready => {
                // We successfully sent the command to start spawning SSH. Now
                // we have to poll the channel to drive the send to completion.
                transition!(PollSendSshSpawn {
                    common: state.common,
                    rx_ssh_ready: rx_ssh_ready,
                    cl_tx: state.cl_tx,
                    cl_rx: state.cl_rx,
                });
            },

            futures::AsyncSink::NotReady((params, _tx_ready)) => {
                // Command not sent; we get our message back.
                state.params = params;
                return Ok(Async::NotReady);
            },
        }
    }

    /// We have successfully started sending the message to the main thread.
    /// Now poll until the message gets there.
    fn poll_poll_send_ssh_spawn<'a>(
        state: &'a mut RentToOwn<'a, PollSendSshSpawn>
    ) -> Poll<AfterPollSendSshSpawn, Error> {
        //println!("poll send ssh spawn");
        let state = state.take();
        try_ready!(state.common.shared().tx_new_ssh.as_mut().unwrap().poll_complete());
        // we finally sent the command; now we wait for the reply
        transition!(WaitingForSshSpawn {
            common: state.common,
            rx_ssh_ready: state.rx_ssh_ready,
            cl_tx: state.cl_tx,
            cl_rx: state.cl_rx,
        });
    }

    /// We have successfully told the main thread to start an SSH process. Now
    /// poll until it tells us the outcome. If/when the SSH start succeeds,
    /// notify the client that we are now ready for bidirectional
    /// communication.
    fn poll_waiting_for_ssh_spawn<'a>(
        state: &'a mut RentToOwn<'a, WaitingForSshSpawn>
    ) -> Poll<AfterWaitingForSshSpawn, Error> {
        //println!("poll waiting for ssh spawn");
        let res = try_ready!(state.rx_ssh_ready.poll());
        //println!("transitioning to communication");
        let new_ssh_info = res?;
        let mut state = state.take();

        if let Ok(AsyncSink::Ready) = state.cl_tx.start_send(ServerMessage::Ok) {
        } else {
            panic!("cmon");
        }

        transition!(CommunicatingForOpen {
            common: state.common,
            cl_tx: state.cl_tx,
            cl_rx: state.cl_rx,
            cl_buf: Vec::new(),
            ssh_tx: new_ssh_info.ptywrite,
            ssh_rx: new_ssh_info.ptyread,
            ssh_buf: Vec::new(),
            ssh_die: new_ssh_info.rx_die.into_future(),
            saw_end: false,
        })
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
