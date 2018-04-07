// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The stund client-server communication protocol.

#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate state_machine_future;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_json;
extern crate tokio_uds;

use failure::Error;
use std::env;
use std::path::PathBuf;

pub mod client;


pub fn get_socket_path() -> Result<PathBuf, Error> {
    let mut p = env::home_dir().ok_or(format_err!("unable to determine your home directory"))?;
    p.push(".ssh");
    p.push("stund.sock");
    Ok(p)
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
    pub host: String
}
