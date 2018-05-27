// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The stund client-server communication protocol.

#[macro_use] extern crate failure;
#[macro_use] extern crate futures;
extern crate libc;
extern crate serde;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate state_machine_future;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_bincode;
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

    /// Close an existing tunnel.
    Close(CloseParameters),

    /// Ask the daemon about its status.
    QueryStatus,

    /// Tell the daemon to exit
    Exit,

    /// End the session.
    Goodbye,
}


#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ServerMessage {
    /// Generic message indicating success with whatever the client was asking
    /// for.
    Ok,

    /// Generic message indicating an error with whatever the client was
    /// asking for.
    Error(String),

    /// Output from an SSH process to be reported to the user by the client.
    SshData(Vec<u8>),

    /// In response to an `Open` message, indicates that this tunnel is
    /// already open.
    TunnelAlreadyOpen,

    /// In response to a `Close` message, indicates that no such tunnel was
    /// open.
    TunnelNotOpen,

    /// In response to a `QueryStatus` message, information about the server
    /// status.
    StatusResponse(StatusInformation),
}


#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenParameters {
    pub host: String
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenResult {
    Success,
    AlreadyOpen
}


#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct CloseParameters {
    pub host: String
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseResult {
    Success,
    NotOpen
}


#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct StatusInformation {
    pub tunnels: Vec<TunnelInformation>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct TunnelInformation {
    pub host: String,
    pub state: TunnelState,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum TunnelState {
    Open,
    Closed,
    Died,
}
