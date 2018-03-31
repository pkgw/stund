// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Interfacing with the daemon.

use bincode;
use chan;
use chan_signal::{self, Signal};
use daemonize;
use failure::Error;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, prelude::*};
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
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        let _r = bincode::serialize_into(&self.sock, &ClientMessage::Goodbye);
    }
}


pub struct Server {
    #[allow(unused)] opts: StundDaemonOptions,
    log: Box<Write>,
    children: HashMap<String, process::Child>,
}

macro_rules! server_log {
    ($server:expr, $fmt:expr) => { $server.log_items(format_args!($fmt)) };
    ($server:expr, $fmt:expr, $($args:tt)*) => { $server.log_items(format_args!($fmt, $($args)*)) };
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

        for (host, mut child) in children.drain() {
            if let Err(e) = child.kill() {
                server_log!(inst, "error killing process for {}: {}", host, e);
            }

            if let Err(e) = child.wait() {
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
        for (host, child) in self.children.iter_mut() {
            if let Some(status) = child.try_wait()? {
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
            match bincode::deserialize_from(&conn)? {
                ClientMessage::Open(params) => { self.handle_open(&conn, params)?; },
                ClientMessage::Exit => { return Ok(Outcome::Exit); } // XXX not waiting for goodbye
                ClientMessage::Goodbye => { break; },
            }
        }

        Ok(Outcome::Continue)
    }

    fn handle_open(&mut self, _conn: &UnixStream, params: OpenParameters) -> Result<(), Error> {
        let child = process::Command::new("ssh")
            .arg("-N")
            .arg(&params.host)
            .spawn()?;

        self.children.insert(params.host, child);
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

    /// Tell the daemon to exit
    Exit,

    /// End the session.
    Goodbye,
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
