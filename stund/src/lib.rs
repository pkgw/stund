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
extern crate pseudotty;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate state_machine_future;
extern crate tokio;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_json;
extern crate tokio_signal;
extern crate tokio_stdin;
extern crate tokio_uds;
extern crate unix_socket;

pub mod protocol;
