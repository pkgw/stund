// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

#![deny(missing_docs)]
#![doc(html_root_url = "https://docs.rs/stund_protocol/0.1.4")]

//! The stund client-server communication protocol.
//!
//! [Stund](https://github.com/pkgw/stund) is a simple daemon for maintaining
//! SSH tunnels. This crate defines the communication protocol used between
//! the CLI client and the daemon that acts as the parent to SSH processes.
//!
//! The protocol only has a handful of commands at the moment. These are
//! defined in this main module. The [`client`] submodule implements the
//! client protocol.

extern crate bytes;
extern crate dirs;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
extern crate libc;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate state_machine_future;
extern crate tokio_codec;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_bincode;
extern crate tokio_uds;

use failure::Error;
use std::path::PathBuf;

pub mod client;
pub mod codecs;

/// Get the path to the Unix domain socket used for client/server communication.
///
/// At the moment, this is fixed to `$HOME/.ssh/stund.sock`.
pub fn get_socket_path() -> Result<PathBuf, Error> {
    let mut p = dirs::home_dir().ok_or(format_err!("unable to determine your home directory"))?;
    p.push(".ssh");
    p.push("stund.sock");
    Ok(p)
}

/// A message that the client may send to the server.
///
/// Some messages are only allowed in certain contexts.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ClientMessage {
    /// Open an SSH tunnel.
    Open(OpenParameters),

    /// User input to be sent to SSH.
    UserData(Vec<u8>),

    /// Close an existing tunnel.
    Close(CloseParameters),

    /// Ask the daemon about its status.
    QueryStatus,

    /// Tell the daemon to exit.
    Exit,

    /// End the session.
    Goodbye,
}

/// A message that the server may send to the client.
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

/// Parameters to the "Open" command.
///
/// This command takes only a single parameter. The model of `stund` is that
/// configuration of details like usernames should be done via the
/// `$HOME/.ssh/config` file, and so are not needed here.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenParameters {
    /// The hostname to which to connect.
    pub host: String,
}

/// Possible outcomes of the "Open" command.
///
/// Besides these outcomes, an error may be signal by the return of a textual
/// error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenResult {
    /// Indicates that the tunnel was successfully opened.
    Success,

    /// Indicates that nothing was done because a tunnel to the specified
    /// host was already open.
    AlreadyOpen,
}

/// Parameters to the "Close" command.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct CloseParameters {
    /// The hostname of the connection to be closed.
    pub host: String,
}

/// Possible outcomes of the "Close" command.
///
/// Besides these outcomes, an error may be signal by the return of a textual
/// error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseResult {
    /// Indicates that the tunnel was successfully opened.
    Success,

    /// Indicates that nothing was done because no tunnel to the specified
    /// host was open.
    NotOpen,
}

/// Information about the current status of the server.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct StatusInformation {
    /// A list of information about the tunnels that have been opened during
    /// this server’s lifetime.
    ///
    /// This list includes tunnels that have been closed, but not any tunnels
    /// opened from any previous invocations of the server.
    pub tunnels: Vec<TunnelInformation>,
}

/// Information about a single tunnel opened by the server.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct TunnelInformation {
    /// The hostname associated with the connection.
    pub host: String,

    /// The current state of the SSH tunnel.
    pub state: TunnelState,
}

/// The state of a single tunnel opened by the server.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum TunnelState {
    /// The tunnel is currently opene.
    Open,

    /// The tunnel was opened but then was manually closed.
    Closed,

    /// The tunnel was opened but died unexpectedly.
    ///
    /// This may be due to something like a connection drop or the user
    /// killing the associated SSH process outside of the server’s knowledge.
    Died,
}
