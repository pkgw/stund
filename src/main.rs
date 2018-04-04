// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

extern crate atty;
extern crate bincode;
extern crate daemonize;
#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate state_machine_future;
#[macro_use] extern crate structopt;
extern crate stund;
extern crate tokio_core;
extern crate tokio_executor;
extern crate tokio_io;
extern crate tokio_pty_process;
extern crate tokio_serde_json;
extern crate tokio_signal;
extern crate tokio_stdin;
extern crate tokio_uds;
extern crate unix_socket;

use failure::Error;
use std::process;
use structopt::StructOpt;

mod daemon;
mod new;


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
        new::new_open(self)?;
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
