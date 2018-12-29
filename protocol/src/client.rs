// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Communication with the daemon.
//!
//! This module provides the [`Connection`] type, which provides a
//! programmatic interface to requests that clients may make of the stund
//! server.

use failure::{Error, ResultExt};
use futures::sink::Send;
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use libc;
use state_machine_future::RentToOwn;
use std::env;
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::process;
use std::thread;
use std::time;
use tokio_core::reactor::Core;
use tokio_uds::UnixStream;

use super::*;
use codecs::{self, Deserializer, Serializer};

type Ser = Serializer<ClientMessage>;
type De = Deserializer<ServerMessage>;
type UserInputStream = Box<Stream<Item = Vec<u8>, Error = io::Error>>;
type UserOutputSink = Box<Sink<SinkItem = Vec<u8>, SinkError = io::Error>>;

/// A connection the stund daemon.
pub struct Connection {
    core: Core,
    ser: Ser,
    de: De,
}

impl Connection {
    fn establish_inner(autolaunch: bool) -> Result<Option<Self>, Error> {
        let core = Core::new().context("couldn't create IO core?")?;
        let handle = core.handle();
        let sock_path = get_socket_path().context("couldn't get path to talk to daemon")?;

        let conn = match UnixStream::connect(&sock_path, &handle) {
            Ok(c) => c,
            Err(_e) => {
                if !autolaunch {
                    // should we care about what the error is exatly?
                    return Ok(None);
                }

                let curr_exe =
                    env::current_exe().context("couldn't get current executable path")?;

                let status = process::Command::new(&curr_exe)
                    .arg("daemon")
                    .status()
                    .context("daemon launcher reported failure")?;

                thread::sleep(time::Duration::from_millis(300));

                if status.success() {
                    UnixStream::connect(&sock_path, &handle)
                        .context("failed to connect to daemon even after launching it")?
                } else {
                    return Err(format_err!("failed to launch background daemon"));
                }
            }
        };

        unsafe {
            // Without turning on linger, I find that the tokio-ized version
            // loses the last bytes of the session. Let's just ignore the
            // return value of setsockopt(), though.
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 2,
            };
            libc::setsockopt(
                conn.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger) as _,
                mem::size_of::<libc::linger>() as libc::socklen_t,
            );
        }

        let (ser, de) = codecs::split(conn);

        Ok(Some(Connection {
            core: core,
            ser: ser,
            de: de,
        }))
    }

    /// Try to connect to the daemon.
    ///
    /// If the daemon is not running, returns `Ok(None)`.
    pub fn try_establish() -> Result<Option<Self>, Error> {
        Self::establish_inner(false)
    }

    /// Connect to the daemon, starting it if it is not already running.
    pub fn establish() -> Result<Self, Error> {
        Ok(Self::establish_inner(true)?.unwrap())
    }

    /// Close the connection to the daemon.
    ///
    /// This operation conducts I/O because it sends a "Goodbye" message.
    pub fn close(mut self) -> Result<(), Error> {
        self.core.run(self.ser.send(ClientMessage::Goodbye))?;
        Ok(())
    }

    /// Tell the daemon to open a new SSH connection.
    ///
    /// Because the user may have to type a password or respond to some other
    /// prompt from the server to authenticate themselves, callers of this
    /// function must provide asynchronous I/O types implementing this user
    /// interaction.
    pub fn send_open<T, R>(
        mut self,
        params: OpenParameters,
        tx_user: T,
        rx_user: R,
    ) -> Result<(OpenResult, Self), Error>
    where
        T: 'static + Sink<SinkItem = Vec<u8>, SinkError = io::Error>,
        R: 'static + Stream<Item = Vec<u8>, Error = io::Error>,
    {
        let fut = self.ser.send(ClientMessage::Open(params));
        let wf = OpenWorkflow::start(fut, self.de, Box::new(tx_user), Box::new(rx_user));
        let (ser, de, result) = self.core.run(wf)?;
        self.ser = ser;
        self.de = de;
        Ok((result, self))
    }

    /// Query the server’s status.
    ///
    /// At the moment, the only information that is returned is a list of
    /// connections that have been opened and their current state.
    pub fn query_status(mut self) -> Result<(StatusInformation, Self), Error> {
        let (ser, de) = (self.ser, self.de);

        let fut = ser
            .send(ClientMessage::QueryStatus)
            .map_err(|e| format_err!("error sending query-status message to daemon: {}", e))
            .and_then(move |ser| {
                de.into_future()
                    .map_err(|(e, _de)| format_err!("error receiving daemon reply: {}", e))
                    .map(|(maybe_msg, de)| (maybe_msg, ser, de))
            })
            .and_then(|(maybe_msg, ser, de)| match maybe_msg {
                Some(ServerMessage::StatusResponse(info)) => Ok((info, ser, de)),
                Some(ServerMessage::Error(msg)) => return Err(format_err!("{}", msg)),
                Some(other) => return Err(format_err!("unexpected server reply: {:?}", other)),
                None => return Err(format_err!("unexpected disconnection from server")),
            });

        let (info, ser, de) = self.core.run(fut)?;
        self.ser = ser;
        self.de = de;
        Ok((info, self))
    }

    /// Tell the server to close an existing tunnel.
    pub fn send_close(mut self, params: CloseParameters) -> Result<(CloseResult, Self), Error> {
        let (ser, de) = (self.ser, self.de);

        let fut = ser
            .send(ClientMessage::Close(params))
            .map_err(|e| format_err!("error sending close message to daemon: {}", e))
            .and_then(move |ser| {
                de.into_future()
                    .map_err(|(e, _de)| format_err!("error receiving daemon reply: {}", e))
                    .map(|(maybe_msg, de)| (maybe_msg, ser, de))
            })
            .and_then(|(maybe_msg, ser, de)| match maybe_msg {
                Some(ServerMessage::Ok) => Ok((CloseResult::Success, ser, de)),
                Some(ServerMessage::TunnelNotOpen) => Ok((CloseResult::NotOpen, ser, de)),
                Some(ServerMessage::Error(msg)) => return Err(format_err!("{}", msg)),
                Some(other) => return Err(format_err!("unexpected server reply: {:?}", other)),
                None => return Err(format_err!("unexpected disconnection from server")),
            });

        let (result, ser, de) = self.core.run(fut)?;
        self.ser = ser;
        self.de = de;
        Ok((result, self))
    }

    /// Tell the server to exit.
    ///
    /// This, of course, means that all SSH tunnels will be closed. The server
    /// will not actually exit until the client sends its "Goodbye" message
    /// and disconnections.
    pub fn send_exit(mut self) -> Result<Self, Error> {
        let (ser, de) = (self.ser, self.de);

        let fut = ser
            .send(ClientMessage::Exit)
            .map_err(|e| format_err!("error sending exit message to daemon: {}", e))
            .and_then(move |ser| {
                de.into_future()
                    .map_err(|(e, _de)| format_err!("error receiving daemon reply: {}", e))
                    .map(|(maybe_msg, de)| (maybe_msg, ser, de))
            })
            .and_then(|(maybe_msg, ser, de)| match maybe_msg {
                Some(ServerMessage::Ok) => Ok((ser, de)),
                Some(ServerMessage::Error(msg)) => return Err(format_err!("{}", msg)),
                Some(other) => return Err(format_err!("unexpected server reply: {:?}", other)),
                None => return Err(format_err!("unexpected disconnection from server")),
            });

        let (ser, de) = self.core.run(fut)?;
        self.ser = ser;
        self.de = de;
        Ok(self)
    }
}

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum OpenWorkflow {
    #[state_machine_future(start, transitions(FirstAck))]
    Issue {
        tx_ssh: Send<Ser>,
        rx_ssh: De,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
    },

    #[state_machine_future(transitions(Finished, Communicating))]
    FirstAck {
        tx_ssh: Ser,
        rx_ssh: De,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
        saw_ok: bool,
    },

    #[state_machine_future(transitions(Finished))]
    Communicating {
        tx_ssh: Ser,
        rx_ssh: De,
        ssh_buf: Vec<u8>,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
        user_buf: Vec<u8>,
    },

    #[state_machine_future(ready)]
    Finished((Ser, De, OpenResult)),

    #[state_machine_future(error)]
    Failed(Error),
}

impl PollOpenWorkflow for OpenWorkflow {
    fn poll_issue<'a>(state: &'a mut RentToOwn<'a, Issue>) -> Poll<AfterIssue, Error> {
        let ser = try_ready!(state.tx_ssh.poll());

        let state = state.take();
        transition!(FirstAck {
            tx_ssh: ser,
            rx_ssh: state.rx_ssh,
            tx_user: state.tx_user,
            rx_user: state.rx_user,
            saw_ok: false,
        })
    }

    fn poll_first_ack<'a>(state: &'a mut RentToOwn<'a, FirstAck>) -> Poll<AfterFirstAck, Error> {
        while let Async::Ready(msg) = state.rx_ssh.poll()? {
            match msg {
                Some(ServerMessage::Ok) => {
                    state.saw_ok = true;
                }

                Some(ServerMessage::Error(text)) => {
                    return Err(format_err!("{}", text));
                }

                Some(ServerMessage::TunnelAlreadyOpen) => {
                    let state = state.take();
                    transition!(Finished((
                        state.tx_ssh,
                        state.rx_ssh,
                        OpenResult::AlreadyOpen
                    )));
                }

                Some(other) => {
                    return Err(format_err!("unexpected response from daemon: {:?}", other));
                }

                None => {
                    return Err(format_err!("connection closed (?)"));
                }
            }
        }

        if state.saw_ok {
            let state = state.take();

            transition!(Communicating {
                rx_user: state.rx_user,
                tx_user: state.tx_user,
                user_buf: Vec::new(),
                tx_ssh: state.tx_ssh,
                rx_ssh: state.rx_ssh,
                ssh_buf: Vec::new(),
            })
        }

        Ok(Async::NotReady)
    }

    fn poll_communicating<'a>(
        state: &'a mut RentToOwn<'a, Communicating>,
    ) -> Poll<AfterCommunicating, Error> {
        // News from the daemon?

        while let Async::Ready(msg) = state.rx_ssh.poll()? {
            match msg {
                Some(ServerMessage::SshData(data)) => {
                    state.user_buf.extend_from_slice(&data);
                }

                Some(ServerMessage::Ok) => {
                    // All done!
                    let mut state = state.take();
                    transition!(Finished((state.tx_ssh, state.rx_ssh, OpenResult::Success)));
                }

                Some(ServerMessage::Error(e)) => {
                    return Err(format_err!("{}", e));
                }

                Some(other) => {
                    return Err(format_err!(
                        "unexpected message from the daemon: {:?}",
                        other
                    ));
                }

                None => {}
            }
        }

        // New text from the user?

        while let Async::Ready(bytes) = state.rx_user.poll()? {
            match bytes {
                None => {
                    // EOF on the user input. This can happen in --no-input mode or,
                    // in principle, if stdin is redirected in some way.
                    break;
                }

                Some(b) => {
                    state.ssh_buf.extend_from_slice(&b);
                }
            }
        }

        // Ready/able to send bytes to the user?

        if state.user_buf.len() != 0 {
            let buf = state.user_buf.clone();

            if let AsyncSink::Ready = state.tx_user.start_send(buf)? {
                state.user_buf.clear();
            }
        }

        // Ready/able to send bytes to the daemon?

        if state.ssh_buf.len() != 0 {
            let buf = state.ssh_buf.clone();

            if let AsyncSink::Ready = state.tx_ssh.start_send(ClientMessage::UserData(buf))? {
                state.ssh_buf.clear();
            }
        }

        // Gotta flush those transmissions.

        try_ready!(state.tx_user.poll_complete());
        try_ready!(state.tx_ssh.poll_complete());
        Ok(Async::NotReady)
    }
}
