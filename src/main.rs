// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

extern crate atty;
extern crate base64;
extern crate daemonize;
#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
extern crate rand;
#[macro_use] extern crate state_machine_future;
#[macro_use] extern crate structopt;
extern crate stund_protocol;
extern crate tokio_borrow_stdio;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_pty_process;
extern crate tokio_serde_json;
extern crate tokio_signal;
extern crate tokio_uds;

use failure::Error;
use std::io;
use std::mem;
use std::process;
use structopt::StructOpt;
use stund_protocol::*;
use stund_protocol::client::Connection;

mod daemon;


#[derive(Debug, StructOpt)]
pub struct StundCloseOptions {
    #[structopt(help = "The host for which the tunnel should be closed.")]
    host: String,
}

impl StundCloseOptions {
    fn cli(self) -> Result<i32, Error> {
        let params = CloseParameters { host: self.host.clone() };

        let conn = Connection::establish()?;
        let (result, conn) = conn.send_close(params)?;

        match result {
            CloseResult::Success => {},

            CloseResult::NotOpen => {
                println!("[No tunnel for \"{}\" was open.]", self.host);
            },
        }

        conn.close()?;
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundDaemonOptions {
    #[structopt(long = "foreground")]
    foreground: bool,
}

impl StundDaemonOptions {
    fn cli(self) -> Result<i32, Error> {
        let d = daemon::State::new(self)?;
        d.serve()?;
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundExitOptions {
}

impl StundExitOptions {
    fn cli(self) -> Result<i32, Error> {
        let conn = match Connection::try_establish()? {
            Some(c) => c,

            None => {
                println!("[Daemon not running; doing nothing.]");
                return Ok(0);
            },
        };

        let conn = conn.send_exit()?;
        conn.close()?;
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundOpenOptions {
    #[structopt(help = "The host for which the tunnel should be opened.")]
    host: String,

    #[structopt(short = "q", long = "quiet", help = "Suppress low-importance UI messages.")]
    quiet: bool,

    // TODO? keepalive option/config setting for tunnels that can/should be
    // restarted by the daemon if the SSH process dies; i.e. ones that do not
    // need interactive authentication to establish. Note that the daemon can
    // autonomously figure out which connections don't need interactive auth:
    // those are the ones for which the login process completes without
    // receiving any user-input messages.
}

impl StundOpenOptions {
    fn cli(self) -> Result<i32, Error> {
        let params = OpenParameters { host: self.host.clone() };

        let conn = Connection::establish()?;

        toggle_terminal_echo(false);
        let r = tokio_borrow_stdio::borrow_stdio(|stdin, stdout| {
            conn.send_open(params, Box::new(stdout), Box::new(stdin))
                .map_err(|_| io::ErrorKind::Other.into())
        });
        toggle_terminal_echo(true);

        let (result, conn) = r?;

        match result {
            OpenResult::Success => {
                if !self.quiet {
                    println!("[Tunnel successfully opened.]");
                }
            },

            OpenResult::AlreadyOpen => {
                if !self.quiet {
                    println!("[Tunnel is already open.]");
                }
            },
        }

        conn.close()?;
        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
pub struct StundStatusOptions {
}

impl StundStatusOptions {
    fn cli(self) -> Result<i32, Error> {
        let conn = Connection::establish()?;
        let (info, conn) = conn.query_status()?;
        conn.close()?;

        if info.tunnels.len() == 0 {
            println!("No tunnels are open.");
        } else {
            let mut longest = 4; // "Host"


            for tun in &info.tunnels {
                longest = longest.max(tun.host.len());
            }

            println!("{:1$}  Status", "Host", longest);
            println!("");

            for tun in &info.tunnels {
                println!("{0:1$}  {2:?}", tun.host, longest, tun.state);
            }
        }

        Ok(0)
    }
}


#[derive(Debug, StructOpt)]
#[structopt(name = "stund", about = "Maintain SSH tunnels in the background.")]
pub enum StundCli {
    #[structopt(name = "close")]
    /// Close an existing SSH tunnel
    Close(StundCloseOptions),

    #[structopt(name = "daemon")]
    /// Manually start the daemon that manages your SSH tunnels
    Daemon(StundDaemonOptions),

    #[structopt(name = "exit")]
    /// Manually tell the daemon to shut down
    Exit(StundExitOptions),

    #[structopt(name = "open")]
    /// Open a new SSH tunnel
    Open(StundOpenOptions),

    #[structopt(name = "status")]
    /// Get information about known SSH tunnels
    Status(StundStatusOptions),
}

impl StundCli {
    fn cli(self) -> Result<i32, Error> {
        match self {
            StundCli::Close(opts) => opts.cli(),
            StundCli::Daemon(opts) => opts.cli(),
            StundCli::Exit(opts) => opts.cli(),
            StundCli::Open(opts) => opts.cli(),
            StundCli::Status(opts) => opts.cli(),
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
