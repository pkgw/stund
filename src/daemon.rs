// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The daemon itself.

use base64;
use daemonize;
use failure::{Error, ResultExt};
use futures::sink::Send;
use futures::stream::{SplitSink, SplitStream, StreamFuture};
use futures::sync::{mpsc, oneshot};
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use libc;
use rand::{self, RngCore};
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
use stund_protocol::codecs::{split, Deserializer, FailureBytesCodec, Serializer};
use stund_protocol::*;
use tokio_codec::{Decoder, Framed};
use tokio_core::reactor::{Core, Handle};
use tokio_pty_process::{AsyncPtyMaster, Child, CommandExt};
use tokio_signal;
use tokio_uds::{UnixListener, UnixStream};

use super::*;

type Ser = Serializer<ServerMessage>;
type De = Deserializer<ClientMessage>;

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
    children: HashMap<String, TunnelState>,
}

macro_rules! log {
    ($state:expr, $fmt:expr) => { $state.log_items(format_args!($fmt)) };
    ($state:expr, $fmt:expr, $($args:tt)*) => { $state.log_items(format_args!($fmt, $($args)*)) };
}

impl State {
    pub fn new(opts: StundDaemonOptions) -> Result<Self, Error> {
        let p = get_socket_path()?;

        if StdUnixStream::connect(&p).is_ok() {
            return Err(format_err!(
                "refusing to start: another daemon is already running"
            ));
        }

        match fs::remove_file(&p) {
            Ok(_) => {}
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {}
                _ => {
                    return Err(e.into());
                }
            },
        }

        // Make sure our socket and logs will be only accessible to us!
        unsafe {
            libc::umask(0o177);
        }

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

            let stream = sig_stream.map_err(|_| {}).and_then(move |sig| {
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

        let server = listener
            .incoming()
            .for_each(move |(socket, sockaddr)| {
                process_client(&handle2, socket, sockaddr, shared.clone(), tx_exit2.clone());
                Ok(())
            })
            .map_err(move |err| {
                log!(shared3.lock().unwrap(), "accept error: {:?}", err);
            });

        handle.spawn(server);

        // The return and error values of the wait-to-die task are
        // meaningless. Note that we don't need to explicitly close our SSH
        // child processes since they'll get SIGHUP'ed when our controlling
        // PTY goes away, which will cause them to exit as desired. Yay Unix!

        let _r = core.run(rx_exit.into_future());
        Ok(())
    }
}

// Supporting jazz for managing SSH processes

type PtyStream = SplitStream<Framed<AsyncPtyMaster, FailureBytesCodec>>;
type PtySink = SplitSink<Framed<AsyncPtyMaster, FailureBytesCodec>>;

enum TunnelState {
    /// An SSH process that we have launched and is, as far as we know, still
    /// running.
    Running { tx_kill: oneshot::Sender<()> },

    /// An SSH process that we launched but is now dead. If the exit status is
    /// `None`, we explicitly killed it; otherwise, it's whatever status the
    /// process died with.
    Exited { status: Option<ExitStatus> },
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
        state: &'a mut RentToOwn<'a, AwaitingChildEvent>,
    ) -> Poll<AfterAwaitingChildEvent, ()> {
        match state.child.poll() {
            Err(_) => {
                return Err(());
            }

            Ok(Async::Ready(status)) => {
                // Child died! We no longer care about any kill messages, but
                // we should let other tasks know what happened.

                let mut state = state.take();
                {
                    let mut sh = state.shared.lock().unwrap();
                    log!(
                        sh,
                        "SSH child for {} unexpectedly died: {:?}",
                        state.key,
                        status
                    );
                    sh.children.insert(
                        state.key,
                        TunnelState::Exited {
                            status: Some(status),
                        },
                    );
                }
                state.rx_kill.close();
                transition!(NotifyingChildDied {
                    tx_die: state.tx_die.send(Some(status)),
                });
            }

            Ok(Async::NotReady) => {}
        }

        match state.rx_kill.poll() {
            Err(_) => {
                return Err(());
            }

            Ok(Async::Ready(_)) => {
                // We've been told to kill the child.
                let mut state = state.take();
                {
                    let mut sh = state.shared.lock().unwrap();
                    log!(sh, "ordered to kill SSH child for {}", state.key);
                    sh.children
                        .insert(state.key, TunnelState::Exited { status: None });
                }
                let _r = state.child.kill(); // can't do anything if this fails
                state.rx_kill.close();
                transition!(NotifyingChildDied {
                    tx_die: state.tx_die.send(None),
                });
            }

            Ok(Async::NotReady) => {}
        }

        Ok(Async::NotReady)
    }

    fn poll_notifying_child_died<'a>(
        state: &'a mut RentToOwn<'a, NotifyingChildDied>,
    ) -> Poll<AfterNotifyingChildDied, ()> {
        match state.tx_die.poll() {
            Err(_) => {
                return Err(());
            }

            Ok(Async::Ready(_)) => {
                transition!(ChildReaped(()));
            }

            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            }
        }
    }
}

// Oh right we actually want to handle clients too

fn process_client(
    handle: &Handle,
    socket: UnixStream,
    addr: SocketAddr,
    shared: Arc<Mutex<State>>,
    tx_exit: mpsc::Sender<()>,
) {
    // Without turning on linger, I find that the tokio-ized version loses
    // the last bytes of the session. Let's just ignore the return value
    // of setsockopt(), though.

    unsafe {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 2,
        };
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger) as _,
            mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }

    let (ser, de) = split(socket);

    let handle2 = handle.clone();
    let shared2 = shared.clone();
    let shared3 = shared.clone();

    let common = ClientCommonState {
        handle: handle.clone(),
        shared: shared,
        _addr: addr,
        tx_exit: tx_exit,
        exit_on_close: false,
    };

    let wrapped = Client::start(common, ser, de)
        .map(move |(common, _ser, _de)| {
            log!(
                shared2.lock().unwrap(),
                "client session finished (exit? {})",
                common.exit_on_close
            );

            if common.exit_on_close {
                handle2.spawn(common.tx_exit.send(()).map(|_| {}).map_err(|_| {}));
            }
        })
        .map_err(move |err| {
            log!(
                shared3.lock().unwrap(),
                "error from client session: {:?}",
                err
            );
        });

    handle.spawn(wrapped);
}

struct ClientCommonState {
    handle: Handle,
    shared: Arc<Mutex<State>>,
    _addr: SocketAddr,
    tx_exit: mpsc::Sender<()>,
    exit_on_close: bool,
}

impl ClientCommonState {
    pub fn shared(&self) -> ::std::sync::MutexGuard<State> {
        self.shared.lock().unwrap()
    }
}

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum Client {
    #[state_machine_future(
        start,
        transitions(CommunicatingForOpen, FinalizingTxn, Finished, Aborting)
    )]
    AwaitingCommand {
        common: ClientCommonState,
        tx: Ser,
        rx: De,
    },

    #[state_machine_future(transitions(Aborting, CommunicatingForOpen, FinalizingTxn))]
    CommunicatingForOpen {
        common: ClientCommonState,
        cl_tx: Ser,
        cl_rx: De,
        cl_buf: Vec<u8>,
        ssh_tx: PtySink,
        ssh_rx: PtyStream,
        ssh_buf: Vec<u8>,
        ssh_key: Vec<u8>,
        ssh_key_status: SshKeyStatus,
        ssh_die: StreamFuture<mpsc::Receiver<Option<ExitStatus>>>,
    },

    #[state_machine_future(transitions(AwaitingCommand))]
    FinalizingTxn {
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
        rx: De,
    },

    #[state_machine_future(error)]
    Failed(Error),
}

/// This is not a "key" like in SSH public key cryptography, but a randomized
/// marker that is printed on the remote host when the login process is
/// completed. We search SSH's output to find this key.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SshKeyStatus {
    Searching(usize),
    FoundIt,
}

impl PollClient for Client {
    fn poll_awaiting_command<'a>(
        state: &'a mut RentToOwn<'a, AwaitingCommand>,
    ) -> Poll<AfterAwaitingCommand, Error> {
        let msg = try_ready!(state.rx.poll());
        let mut state = state.take();

        match msg {
            None => {
                // Stream ended. (= connection closed?)
                transition!(Finished((state.common, state.tx, state.rx)));
            }

            Some(ClientMessage::Close(params)) => {
                return process_close_command(state.common, params, state.tx, state.rx);
            }

            Some(ClientMessage::Open(params)) => {
                return process_open_command(state.common, params, state.tx, state.rx);
            }

            Some(ClientMessage::Exit) => {
                // To be able to close out this connection in a nice way, when we get
                // this command we set a flag that will cause the exit message to be
                // sent on connection close.

                log!(
                    state.common.shared(),
                    "commanded to exit after client disconnects"
                );
                state.common.exit_on_close = true;
                let send = state.tx.send(ServerMessage::Ok);

                transition!(FinalizingTxn {
                    common: state.common,
                    tx: send,
                    rx: state.rx,
                });
            }

            Some(ClientMessage::Goodbye) => {
                transition!(Finished((state.common, state.tx, state.rx)));
            }

            Some(ClientMessage::QueryStatus) => {
                return process_status_query(state.common, state.tx, state.rx);
            }

            Some(other) => {
                return Err(format_err!("unexpected message from client: {:?}", other));
            }
        }
    }

    /// The main thread has successfully started SSH! Now we do some
    /// uber-multiplexing to allow the client to communicate with the SSH
    /// process interactively, while keeping tabs on whether SSH bites the
    /// dust under us.
    fn poll_communicating_for_open<'a>(
        state: &'a mut RentToOwn<'a, CommunicatingForOpen>,
    ) -> Poll<AfterCommunicatingForOpen, Error> {
        // New text from the user?

        while let Async::Ready(msg) = state.cl_rx.poll()? {
            match msg {
                Some(ClientMessage::UserData(data)) => {
                    state.ssh_buf.extend_from_slice(&data);
                }

                Some(other) => {
                    // Could consider aborting here, but if we didn't
                    // understand the client then probably there's
                    // something messed up about the channel.
                    return Err(format_err!(
                        "unexpected message from the client: {:?}",
                        other
                    ));
                }

                None => {
                    return Err(format_err!("client connection unexpectedly closed"));
                }
            }
        }

        // New text from SSH?

        loop {
            let outcome = match state.ssh_rx.poll() {
                Ok(x) => x,
                Err(e) => {
                    let msg = format!(
                        "something went wrong communicating with the SSH process: {}",
                        e
                    );
                    let mut state = state.take();
                    transition!(abort_client(state.common, state.cl_tx, state.cl_rx, msg));
                }
            };

            match outcome {
                Async::NotReady => break,

                Async::Ready(maybe_bytes) => {
                    if let Some(bytes) = maybe_bytes {
                        // We need to search SSH's output for the "key" that
                        // we use to figure out that login has completed
                        // successfully.
                        //
                        // If we were cleverer we would not show the "key"
                        // text to the client, but I don't want to figure out
                        // the right state machine magic to ensure that output
                        // is eventually showed in the event of an incomplete
                        // match.
                        if let SshKeyStatus::Searching(next_idx) = state.ssh_key_status {
                            let mut n = next_idx;

                            for b in &bytes {
                                if b == state.ssh_key[n] {
                                    n += 1;

                                    if n == state.ssh_key.len() {
                                        break;
                                    }
                                } else {
                                    n = 0;
                                }
                            }

                            if n == state.ssh_key.len() {
                                state.ssh_key_status = SshKeyStatus::FoundIt;
                            } else {
                                state.ssh_key_status = SshKeyStatus::Searching(n);
                            }
                        }

                        state.cl_buf.extend_from_slice(&bytes);
                    } else {
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

        // What's next?

        if let SshKeyStatus::FoundIt = state.ssh_key_status {
            let state = state.take();

            hand_off_ssh_process(
                &state.common.handle,
                state.common.shared.clone(),
                state.ssh_tx,
                state.ssh_rx,
            );

            let send = state.cl_tx.send(ServerMessage::Ok);
            transition!(FinalizingTxn {
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
    fn poll_finalizing_txn<'a>(
        state: &'a mut RentToOwn<'a, FinalizingTxn>,
    ) -> Poll<AfterFinalizingTxn, Error> {
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
    fn poll_aborting<'a>(state: &'a mut RentToOwn<'a, Aborting>) -> Poll<AfterAborting, Error> {
        try_ready!(state.tx.poll());
        Err(format_err!(
            "ending connection now that client has been notified"
        ))
    }
}

fn process_open_command(
    common: ClientCommonState,
    params: OpenParameters,
    mut tx: Ser,
    rx: De,
) -> Poll<AfterAwaitingCommand, Error> {
    let never_mind = {
        let mut sh = common.shared();
        log!(sh, "got command to spawn SSH for {}", params.host);

        if let Some(&TunnelState::Running { .. }) = sh.children.get(&params.host) {
            log!(sh, "tunnel already open -- notifying client");
            true
        } else {
            false
        }
    };

    if never_mind {
        let send = tx.send(ServerMessage::TunnelAlreadyOpen);
        transition!(FinalizingTxn {
            common,
            tx: send,
            rx
        });
    }

    // Generate a magic bit of text that we'll use to recognize when the
    // login has succeeded.

    let mut rng = rand::thread_rng();
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    let key = format!("STUND:{}", base64::encode(&buf));

    // Let's launch the process.

    let (tx_die, rx_die) = mpsc::channel(0);

    fn inner(
        common: &ClientCommonState,
        params: &OpenParameters,
        tx_die: mpsc::Sender<Option<ExitStatus>>,
        key: &str,
    ) -> Result<Framed<AsyncPtyMaster, FailureBytesCodec>, Error> {
        let (tx_kill, rx_kill) = oneshot::channel();
        let ptymaster = AsyncPtyMaster::open().context("failed to create PTY")?;

        // The -t arg allocates a PTY for the command so that "tail" will die
        // with a SIGHUP when SSH dies. Otherwise it will linger forever!

        let child = process::Command::new("ssh")
            .arg("-t")
            .arg(&params.host)
            .arg(format!("echo \"{}\" && exec tail -f /dev/null", key))
            .env_remove("DISPLAY")
            .spawn_pty_async(&ptymaster)
            .context("failed to launch SSH")?;

        // The task that will remember this child and wait around for it die.

        common.handle.spawn(ChildMonitor::start(
            common.shared.clone(),
            params.host.clone(),
            child,
            rx_kill,
            tx_die,
        ));

        // The kill channel gives us a way to control the process later. We hold
        // on to the handles to the ptymaster and rx_die for now, because we care
        // about them when completing the password entry stage of the daemon
        // setup.

        common.shared().children.insert(
            params.host.clone(),
            TunnelState::Running { tx_kill: tx_kill },
        );

        Ok(FailureBytesCodec::new().framed(ptymaster))
    }

    match inner(&common, &params, tx_die, &key) {
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
                ssh_key: key.into_bytes(),
                ssh_key_status: SshKeyStatus::Searching(0),
                ssh_die: rx_die.into_future(),
            });
        }

        Err(e) => {
            let msg = format!("failed to launch SSH: {}", e);
            transition!(abort_client(common, tx, rx, msg));
        }
    }
}

// A task for monitoring each SSH process's PTY once it has successfully
// finished the password entry phase.

fn hand_off_ssh_process(
    handle: &Handle,
    shared: Arc<Mutex<State>>,
    _ssh_tx: PtySink,
    ssh_rx: PtyStream,
) {
    //println!("handing off SSH process to monitor");
    let shared2 = shared.clone();

    let ssh_monitor = ssh_rx
        .for_each(move |bytes| {
            log!(shared.lock().unwrap(), "SSH: {:?}", bytes);
            Ok(())
        })
        .map_err(move |err| {
            log!(shared2.lock().unwrap(), "error polling SSH: {}", err);
        });

    handle.spawn(ssh_monitor);
}

fn process_close_command(
    common: ClientCommonState,
    params: CloseParameters,
    tx: Ser,
    rx: De,
) -> Poll<AfterAwaitingCommand, Error> {
    log!(
        common.shared(),
        "got command to close tunnel SSH for {}",
        params.host
    );

    let tx_kill = match common.shared().children.remove(&params.host) {
        Some(TunnelState::Running { tx_kill }) => Some(tx_kill),
        Some(TunnelState::Exited { .. }) | None => None,
    };

    let tx_kill = match tx_kill {
        Some(t) => t,
        None => {
            log!(common.shared(), "no such tunnel -- notifying client");
            let send = tx.send(ServerMessage::TunnelNotOpen);
            transition!(FinalizingTxn {
                common,
                tx: send,
                rx
            });
        }
    };

    if let Err(_) = tx_kill.send(()) {
        let msg = "failed to send internal kill signal (?)".to_owned();
        transition!(abort_client(common, tx, rx, msg));
    }

    let send = tx.send(ServerMessage::Ok);
    transition!(FinalizingTxn {
        common,
        tx: send,
        rx
    });
}

fn process_status_query(
    common: ClientCommonState,
    tx: Ser,
    rx: De,
) -> Poll<AfterAwaitingCommand, Error> {
    let mut info = StatusInformation {
        tunnels: Vec::new(),
    };

    for (host, tinfo) in common.shared().children.iter() {
        let state = match tinfo {
            &TunnelState::Running { .. } => super::TunnelState::Open,
            &TunnelState::Exited { status: None } => super::TunnelState::Closed,
            &TunnelState::Exited { status: _other } => super::TunnelState::Died,
        };

        info.tunnels.push(TunnelInformation {
            host: host.clone(),
            state: state,
        });
    }

    let send = tx.send(ServerMessage::StatusResponse(info));
    transition!(FinalizingTxn {
        common,
        tx: send,
        rx
    });
}

/// This function used to be much more elaborate; it can probably be ditched
/// now.
fn abort_client(common: ClientCommonState, tx: Ser, rx: De, message: String) -> Aborting {
    Aborting {
        common: common,
        tx: tx.send(ServerMessage::Error(message)),
        rx: rx,
    }
}
