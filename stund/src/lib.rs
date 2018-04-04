// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The main CLI driver logic.

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
extern crate tokio_signal;
extern crate tokio_stdin;
extern crate tokio_uds;

pub mod protocol;
