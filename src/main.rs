// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

extern crate bincode;
#[macro_use] extern crate chan;
extern crate chan_signal;
extern crate daemonize;
#[macro_use] extern crate failure;
extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate ssh;
#[macro_use] extern crate structopt;
extern crate unix_socket;

use failure::Error;
use std::process;
use structopt::StructOpt;

mod daemon;


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
}

impl StundOpenOptions {
    fn cli(self) -> Result<i32, Error> {
        let mut conn = daemon::DaemonConnection::new()?;
        conn.send_message(&daemon::ClientMessage::Open(self.into()))?;
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
