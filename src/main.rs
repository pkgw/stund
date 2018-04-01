// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

extern crate atty;
extern crate bincode;
extern crate byteorder;
#[macro_use] extern crate chan;
extern crate chan_signal;
extern crate daemonize;
#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate state_machine_future;
#[macro_use] extern crate structopt;
extern crate tokio;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_json;
extern crate tokio_signal;
extern crate tokio_stdin;
extern crate tokio_uds;
extern crate unix_socket;

use failure::Error;
use std::io::{self, prelude::*};
use std::mem;
use std::process;
use std::thread;
use structopt::StructOpt;

mod daemon;
mod new;
mod pty;

use daemon::ServerMessage;


#[derive(Debug, StructOpt)]
pub struct StundDaemonOptions {
    #[structopt(long = "foreground")]
    foreground: bool,
}

impl StundDaemonOptions {
    fn cli(self) -> Result<i32, Error> {
        daemon::Server::launch(self)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundExitOptions {
}

impl StundExitOptions {
    fn cli(self) -> Result<i32, Error> {
        // Note that if the daemon isn't running this will be dumb and start
        // it ...
        let mut conn = daemon::DaemonConnection::new()?;
        conn.send_message(&daemon::ClientMessage::Exit)?;
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundOpenOptions {
    #[structopt(help = "The host for which the tunnel should be opened.")]
    host: String,

    // TODO? keepalive option/config setting for tunnels that can/should be
    // restarted by the daemon if the SSH process dies; i.e. ones that do not
    // need interactive authentication to establish.
}

fn toggle_terminal_echo(active: bool) {
    if atty::isnt(atty::Stream::Stdout) {
        return;
    }

    let mut attrs: libc::termios = unsafe { mem::zeroed() };

    if unsafe { libc::tcgetattr(0, &mut attrs as _) } != 0 {
        println!("error querying terminal attributes?!");
        return;
    }

    if active {
        attrs.c_lflag |= libc::ECHO;
    } else {
        attrs.c_lflag &= !libc::ECHO;
    }
    
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &attrs as _) } != 0 {
        println!("error setting terminal attributes?!");
    }
}

impl StundOpenOptions {
    fn cli(self) -> Result<i32, Error> {
        new::new_open(self)?;
        Ok(0)
    }

    fn _oldcli(self) -> Result<i32, Error> {
        let mut conn = daemon::DaemonConnection::new()?;
        conn.send_message(&daemon::ClientMessage::Open((&self).into()))?;

        match conn.recv_message()? {
            daemon::ServerMessage::Ok => {},

            daemon::ServerMessage::Error(e) => {
                return Err(format_err!("{}", e).context("error opening tunnel").into());
            },

            other => {
                return Err(format_err!("unexpected response from daemon: {:?}", other)
                           .context("error opening tunnel").into());
            },
        }

        println!("[Begin SSH login; type \".\" on an empty line when login has succeeded.]");
        toggle_terminal_echo(false);
        
        // Now we get to hang out and forward the user's keypresses to SSH.
        // Thread 1 reads from the terminal.

        let (sdata1, rdata1) = chan::async();
        let (serror1, rerror1) = chan::async();
        let (squit1, rquit1) = chan::sync::<()>(0);

        thread::spawn(move || {
            let mut buf = [0u8; 512];
            let mut stdin = io::stdin();

            loop {
                let n = match stdin.read(&mut buf) {
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

        // Thread 2 reads from the server.

        let sock_clone = conn.clone_socket();
        let (sdata2, rdata2) = chan::async();
        let (serror2, rerror2) = chan::async();
        let (squit2, rquit2) = chan::sync::<()>(0);

        thread::spawn(move || {
            loop {
                let msg = match bincode::deserialize_from(&sock_clone) {
                    Ok(m) => m,
                    Err(e) => {
                        serror2.send(e);
                        break;
                    },
                };

                println!("msg: {:?}", msg);
                sdata2.send(msg);

                chan_select! {
                    default => {},
                    rquit2.recv() => { break; },
                }
            }

            mem::forget(sock_clone);
        });

        // And now we orchestrate it all.

        let mut stdout = io::stdout();

        loop {
            chan_select! {
                rdata1.recv() -> o => {
                    if let Some(tty_data) = o {
                        if tty_data == &[46, 10] {
                            break;
                        }

                        conn.send_message(&daemon::ClientMessage::UserData(tty_data))?;
                    }
                },

                rdata2.recv() -> o => {
                    if let Some(msg) = o {
                        match msg {
                            ServerMessage::Error(e) => {
                                return Err(format_err!("the daemon reported an error: {}", e));
                            },

                            ServerMessage::SshData(ref data) => {
                                stdout.write_all(data)?;
                                stdout.flush()?;
                            },

                            other => {
                                return Err(format_err!("unexpected daemon message during SSH setup: {:?}", other));
                            },
                        }
                    }
                },

                rerror1.recv() -> o => {
                    if let Some(err) = o {
                        return Err(format_err!("error reading from terminal: {}", err));
                    }
                },

                rerror2.recv() -> o => {
                    if let Some(err) = o {
                        return Err(format_err!("error reading from daemon: {}", err));
                    }
                },
            }
        }

        toggle_terminal_echo(true);
        mem::drop(squit1); // cause the rquit1 channel to sync
        mem::drop(squit2);
        conn.send_message(&daemon::ClientMessage::EndOfUserData)?;

        match conn.recv_message()? {
            daemon::ServerMessage::Ok => {},

            daemon::ServerMessage::Error(e) => {
                return Err(format_err!("{}", e).context("error opening tunnel").into());
            },

            other => {
                return Err(format_err!("unexpected response from daemon: {:?}", other)
                           .context("error opening tunnel").into());
            },
        }

        println!("[Success]");
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
#[structopt(name = "stund", about = "Maintain SSH tunnels in the background.")]
pub enum StundCli {
    #[structopt(name = "daemon")]
    /// Manually start the daemon that manages your SSH tunnels
    Daemon(StundDaemonOptions),

    #[structopt(name = "exit")]
    /// Manually tell the daemon to shut down
    Exit(StundExitOptions),

    #[structopt(name = "open")]
    /// Open a new SSH tunnel
    Open(StundOpenOptions),
}

impl StundCli {
    fn cli(self) -> Result<i32, Error> {
        match self {
            StundCli::Daemon(opts) => opts.cli(),
            StundCli::Exit(opts) => opts.cli(),
            StundCli::Open(opts) => opts.cli(),
        }
    }
}


fn main() {
    let program = StundCli::from_args();

    process::exit(match program.cli() {
        Ok(code) => code,

        Err(e) => {
            eprintln!("fatal error in stund");
            for cause in e.causes() {
                eprintln!("  caused by: {}", cause);
            }
            1
        },
    });
}
