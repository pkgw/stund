// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Interfacing with the daemon.

use bincode;
use chan;
use chan_signal::{self, Signal};
use daemonize;
use failure::{Error, ResultExt};
use pty::{self, CommandPtyExt};
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, prelude::*};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time;
use super::{StundDaemonOptions, StundOpenOptions};
use unix_socket::{UnixListener, UnixStream};


fn get_socket_path() -> Result<PathBuf, Error> {
    let mut p = env::home_dir().ok_or(format_err!("unable to determine your home directory"))?;
    p.push(".ssh");
    p.push("stund.sock");
    Ok(p)
}

fn get_log_path() -> Result<PathBuf, Error> {
    let mut p = env::home_dir().ok_or(format_err!("unable to determine your home directory"))?;
    p.push(".ssh");
    p.push("stund.log");
    Ok(p)
}

pub struct DaemonConnection {
    sock: UnixStream
}


impl DaemonConnection {
    /// Connect to the daemon, starting it if it doesn't seem to be running.
    pub fn new() -> Result<Self, Error> {
        let p = get_socket_path()?;

        let sock = match UnixStream::connect(&p) {
            Ok(sock) => sock,
            Err(e) => {
                match e.kind() {
                    io::ErrorKind::NotFound => {
                        // Looks like the daemon isn't running. Start it and try again.
                        println!("stund: launching background daemon");
                        let argv0 = env::args().next().unwrap_or_else(|| "stund".to_owned());

                        if !process::Command::new(argv0).arg("daemon").status()?.success() {
                            return Err(format_err!("failed to launch the daemon"));
                        }

                        thread::sleep(time::Duration::from_millis(500));
                        UnixStream::connect(&p)?
                    },

                    _ => {
                        return Err(e.into());
                    },
                }
            },
        };

        Ok(DaemonConnection {
            sock: sock,
        })
    }

    pub fn send_message(&mut self, msg: &ClientMessage) -> Result<(), Error> {
        bincode::serialize_into(&self.sock, msg)?;
        Ok(())
    }

    pub fn recv_message(&mut self) -> Result<ServerMessage, Error> {
        Ok(bincode::deserialize_from(&self.sock)?)
    }

    // Hack hack hack
    pub fn clone_socket(&self) -> UnixStream {
        unsafe { UnixStream::from_raw_fd(self.sock.as_raw_fd()) }
    }
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        let _r = bincode::serialize_into(&self.sock, &ClientMessage::Goodbye);
    }
}


pub struct Server {
    #[allow(unused)] opts: StundDaemonOptions,
    log: Box<Write>,
    children: HashMap<String, Tunnel>,
}

struct Tunnel {
    ssh: process::Child,

    /// Needed to maintain the SSH daemon's PTY! Otherwise it SIGHUPs and dies
    /// if it tries to do any output.
    #[allow(unused)] ptymaster: fs::File,
}

macro_rules! server_log {
    ($server:expr, $fmt:expr) => { $server.log_items(format_args!($fmt)) };
    ($server:expr, $fmt:expr, $($args:tt)*) => { $server.log_items(format_args!($fmt, $($args)*)) };
}

macro_rules! server_error {
    ($fmt:expr) => { ServerMessage::Error(format!($fmt)) };
    ($fmt:expr, $($args:tt)*) => { ServerMessage::Error(format!($fmt, $($args)*)) };
}

impl Server {
    pub fn launch(opts: StundDaemonOptions) -> Result<i32, Error> {
        let p = get_socket_path()?;

        if UnixStream::connect(&p).is_ok() {
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

        let sock = UnixListener::bind(&p)?;
        let (sconn, rconn) = chan::async();

        let log: Box<Write> = if opts.foreground {
            println!("stund daemon: staying in foreground");
            Box::new(io::stdout())
        } else {
            let log = fs::File::create(get_log_path()?)?;
            daemonize::Daemonize::new().start()?;
            Box::new(log)
        };

        let signal = chan_signal::notify(&[
            Signal::ABRT, Signal::BUS, Signal::CHLD, Signal::HUP, Signal::ILL, Signal::INT,
            Signal::KILL, Signal::PIPE, Signal::SEGV, Signal::TERM, Signal::QUIT,
        ]);

        let mut inst = Server {
            opts: opts,
            log: log,
            children: HashMap::new(),
        };

        thread::spawn(move || {
            for item in sock.incoming() {
                sconn.send(item);
            }
        });

        server_log!(inst, "stund daemon: starting main loop");

        loop {
            let mut event = Event::Nothing;

            chan_select! {
                signal.recv() -> signal => {
                    if let Some(signal) = signal {
                        event = Event::Signal(signal);
                    }
                },

                rconn.recv() -> conn => {
                    if let Some(r) = conn {
                        event = Event::Connection(r);
                    }
                }
            }

            let desc = format!("{:?}", event);

            let outcome = match inst.handle_event(event) {
                Ok(oc) => oc,
                Err(e) => {
                    server_log!(inst, "error while handling event {}", desc);
                    for cause in e.causes() {
                        server_log!(inst, "  caused by: {}", cause);
                    }
                    Outcome::Continue
                }
            };

            if let Outcome::Exit = outcome {
                break;
            }
        }

        server_log!(inst, "stund daemon: wrapping up");

        let mut children = HashMap::new();
        ::std::mem::swap(&mut inst.children, &mut children); // borrowck fun

        for (host, mut tunnel) in children.drain() {
            if let Err(e) = tunnel.ssh.kill() {
                server_log!(inst, "error killing process for {}: {}", host, e);
            }

            if let Err(e) = tunnel.ssh.wait() {
                server_log!(inst, "error wait()ing for process for {}: {}", host, e);
            }
        }

        fs::remove_file(&p)?;
        Ok(0)
    }

    fn log_items(&mut self, args: fmt::Arguments) {
        let _r = writeln!(self.log, "{}", args);
    }

    fn handle_event(&mut self, event: Event) -> Result<Outcome, Error> {
        match event {
            Event::Nothing => {
                Ok(Outcome::Continue)
            },

            Event::Signal(Signal::CHLD) => {
                self.handle_sigchld()?;
                Ok(Outcome::Continue)
            },

            Event::Signal(signal) => {
                server_log!(self, "exiting on fatal signal {:?}", signal);
                Ok(Outcome::Exit)
            },

            Event::Connection(rconn) => {
                self.handle_client(rconn?)
            },
        }
    }

    /// Probably a better way to do this stuff but can't figure out something
    /// that works with the lifetimes.
    fn find_exited_child(&mut self) -> Result<(String, process::ExitStatus), Error> {
        for (host, tunnel) in self.children.iter_mut() {
            if let Some(status) = tunnel.ssh.try_wait()? {
                return Ok((host.to_owned(), status));
            }
        }

        Err(format_err!("no child actually exited?"))
    }

    fn handle_sigchld(&mut self) -> Result<(), Error> {
        let (host, status) = self.find_exited_child()?;
        self.children.remove(&host);
        server_log!(self, "process for {} exited with status {:?}", host, status);
        // TODO: try to reconnect? But we have no TTY ...
        Ok(())
    }

    fn handle_client(&mut self, conn: UnixStream) -> Result<Outcome, Error> {
        loop {
            let msg = bincode::deserialize_from(&conn)?;

            let r = match msg {
                ClientMessage::Open(params) => { self.handle_open(&conn, params) },
                ClientMessage::Exit => { return Ok(Outcome::Exit); } // XXX not waiting for goodbye
                ClientMessage::Goodbye => { break; },
                other => {
                    server_log!(self, "unexpected toplevel message from client: {:?}", other);
                    break;
                }
            };

            if let Err(e) = r {
                if let Err(e2) = bincode::serialize_into(conn, &server_error!("{}", e)) {
                    server_log!(self, "error doing work for client to be reported next");
                    server_log!(self, "additional error encountered reporting error to client: {}", e2);
                }

                return Err(e.context("error reported to client").into());
            }
        }

        Ok(Outcome::Continue)
    }

    fn handle_open(&mut self, conn: &UnixStream, params: OpenParameters) -> Result<(), Error> {
        // Start with the PTY.

        let mut ptymaster = pty::create().context("failed to create PTY")?;

        let child = process::Command::new("ssh")
            .arg("-N")
            .arg(&params.host)
            .env_remove("DISPLAY")
            .slave_to_pty(&ptymaster).context("failed to set up PTY slave")?
            .spawn().context("failed to launch SSH command")?;

        // To reap children properly, we need to log them here, before we know
        // if we've actually set up the SSH session fully.
        self.children.insert(params.host, Tunnel {
            ssh: child,
            ptymaster: ptymaster.try_clone().context("failed to duplicate PTY master")?,
        });

        // Next, forward any I/O needed to execute the login. It feels pretty
        // dumb to use threads for this but meh. The initial OK message tells
        // the client that it can start forwarding I/O.

        bincode::serialize_into(conn, &ServerMessage::Ok)?;

        // Thread 1 reads from SSH.

        let mut master_clone = ptymaster.try_clone().context("failed to duplicate PTY master")?;
        let (sdata1, rdata1) = chan::async();
        let (serror1, rerror1) = chan::async();
        let (squit1, rquit1) = chan::sync::<()>(0);

        thread::spawn(move || {
            let mut buf = [0u8; 512];

            loop {
                let n = match master_clone.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        serror1.send(e);
                        break;
                    },
                };

                sdata1.send(buf[..n].to_owned());
                
                chan_select! {
                    default => {},
                    rquit1.recv() => { break; },
                }
            }
        });

        // Thread 2 reads from the client

        let conn_clone = unsafe { UnixStream::from_raw_fd(conn.as_raw_fd()) };
        let (sdata2, rdata2) = chan::async();
        let (serror2, rerror2) = chan::async();
        let (squit2, rquit2) = chan::sync::<()>(0);

        thread::spawn(move || {
            loop {
                let msg = match bincode::deserialize_from(&conn_clone) {
                    Ok(m) => m,
                    Err(e) => {
                        serror2.send(e);
                        break;
                    },
                };

                sdata2.send(msg);

                chan_select! {
                    default => {},
                    rquit2.recv() => { break; },
                }
            }

            mem::forget(conn_clone);
        });

        // And now we orchestrate it all.

        loop {
            chan_select! {
                rdata1.recv() -> o => {
                    if let Some(ssh_data) = o {
                        //println!("rdata1: {:?}", ssh_data);
                        bincode::serialize_into(conn, &ServerMessage::SshData(ssh_data))?;
                    }
                },

                rdata2.recv() -> o => {
                    if let Some(msg) = o {
                        //println!("rdata2: {:?}", msg);
                        match msg {
                            ClientMessage::EndOfUserData => {
                                break;
                            },

                            ClientMessage::UserData(ref data) => {
                                ptymaster.write_all(data)?;
                            },

                            other => {
                                server_log!(self, "unexpected client message during SSH setup: {:?}", other);
                                break;
                            },
                        }
                    }
                },

                rerror1.recv() -> o => {
                    if let Some(err) = o {
                        server_log!(self, "error reading from SSH: {}", err);
                        break;
                    }
                },

                rerror2.recv() -> o => {
                    if let Some(err) = o {
                        server_log!(self, "error reading from client: {}", err);
                        break;
                    }
                },
            }
        }

        mem::drop(squit1); // cause the rquit1 channel to sync
        mem::drop(squit2);

        bincode::serialize_into(conn, &ServerMessage::Ok)?;
        Ok(())
    }
}

#[derive(Debug)]
enum Event {
    Signal(chan_signal::Signal),
    Connection(Result<UnixStream, io::Error>),
    Nothing,
}

#[derive(Debug)]
enum Outcome {
    Continue,
    Exit,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ClientMessage {
    /// Open an SSH tunnel
    Open(OpenParameters),

    /// User input to be sent to SSH
    UserData(Vec<u8>),

    /// User input has concluded.
    EndOfUserData,

    /// Tell the daemon to exit
    Exit,

    /// End the session.
    Goodbye,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ServerMessage {
    Ok,
    SshData(Vec<u8>),
    Error(String),
}


#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenParameters {
    host: String
}

impl From<StundOpenOptions> for OpenParameters {
    fn from(opts: StundOpenOptions) -> Self {
        OpenParameters {
            host: opts.host,
        }
    }
}
