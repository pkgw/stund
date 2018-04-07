// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

extern crate atty;
extern crate daemonize;
#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
#[macro_use] extern crate state_machine_future;
#[macro_use] extern crate structopt;
extern crate stund;
extern crate tokio_borrow_stdio;
extern crate tokio_core;
extern crate tokio_file_unix;
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
use stund::protocol::OpenParameters;
use stund::protocol::client::Connection;

mod daemon;
//mod new;


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
        println!("TODO");
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

impl StundOpenOptions {
    fn cli(self) -> Result<i32, Error> {
        let params = OpenParameters { host: self.host.clone() };

        let mut conn = Connection::establish()?;

        println!("[Log in and type \".\" on its own line when finished.]");

        toggle_terminal_echo(false);
        let r = tokio_borrow_stdio::borrow_stdio(|stdin, stdout| {
            conn.send_open(params, Box::new(stdout), Box::new(stdin))
                .map_err(|_| io::ErrorKind::Other.into())
        });
        toggle_terminal_echo(true);

        conn = r?;
        println!("[Success!]");

        conn.close()?;
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
