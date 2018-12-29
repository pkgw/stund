// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! The dumbest example.

extern crate futures;
extern crate tokio_borrow_stdio;
extern crate tokio_core;

use futures::stream::Stream;
use futures::Future;
use std::str;
use tokio_core::reactor::Core;

fn main() {
    let r = tokio_borrow_stdio::borrow_stdio(|stdin, stdout| {
        let mut core = Core::new().expect("couldn't create core");

        core.run(
            stdin
                .map(move |data| {
                    let text = match str::from_utf8(&data) {
                        Ok(s) => format!("YOU WROTE: {}", s),
                        Err(_e) => format!("YOU WROTE: {:?} [couldn't decode as UTF8]", data),
                    };

                    text.into_bytes()
                })
                .forward(stdout)
                .map(|_| {}),
        )
    });

    println!("final: {:?}", r);
}
