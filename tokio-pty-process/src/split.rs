// Copyright 2018 Peter Williams, Tokio Contributors
// Copyright 2019 Fabian Freyer
// Licensed under both the MIT License and the Apache-2.0 license.

//! This module is a clone of
//! <https://github.com/tokio-rs/tokio/blob/master/tokio-io/src/split.rs>
//! (commit 1119d57), modified to refer to our AsyncPtyMaster types. We need
//! to implement the splitting ourselves in order to be able to implement
//! AsRawFd for the split types.

use bytes::{Buf, BufMut};
use futures::sync::BiLock;
use futures::{try_ready, Async, Poll};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use tokio_io::{AsyncRead, AsyncWrite};

use crate::{AsAsyncPtyFd, AsyncPtyMaster};

pub fn split(master: AsyncPtyMaster) -> (AsyncPtyMasterReadHalf, AsyncPtyMasterWriteHalf) {
    let (a, b) = BiLock::new(master);
    (
        AsyncPtyMasterReadHalf { handle: a },
        AsyncPtyMasterWriteHalf { handle: b },
    )
}

/// Read half of a AsyncPtyMaster, created with AsyncPtyMaster::split.
pub struct AsyncPtyMasterReadHalf {
    handle: BiLock<AsyncPtyMaster>,
}

/// Write half of a AsyncPtyMaster, created with AsyncPtyMaster::split.
pub struct AsyncPtyMasterWriteHalf {
    handle: BiLock<AsyncPtyMaster>,
}

impl AsAsyncPtyFd for AsyncPtyMasterReadHalf {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        let l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        Ok(Async::Ready(l.as_raw_fd()))
    }
}

impl AsAsyncPtyFd for &AsyncPtyMasterReadHalf {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        let l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        Ok(Async::Ready(l.as_raw_fd()))
    }
}

impl AsAsyncPtyFd for &mut AsyncPtyMasterReadHalf {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        let l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        Ok(Async::Ready(l.as_raw_fd()))
    }
}

impl AsAsyncPtyFd for &AsyncPtyMasterWriteHalf {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        let l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        Ok(Async::Ready(l.as_raw_fd()))
    }
}

impl AsAsyncPtyFd for &mut AsyncPtyMasterWriteHalf {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        let l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        Ok(Async::Ready(l.as_raw_fd()))
    }
}

fn would_block() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "would block")
}

impl Read for AsyncPtyMasterReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.handle.poll_lock() {
            Async::Ready(mut l) => l.read(buf),
            Async::NotReady => Err(would_block()),
        }
    }
}

impl AsyncRead for AsyncPtyMasterReadHalf {
    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        let mut l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        l.read_buf(buf)
    }
}

impl Write for AsyncPtyMasterWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.handle.poll_lock() {
            Async::Ready(mut l) => l.write(buf),
            Async::NotReady => Err(would_block()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.handle.poll_lock() {
            Async::Ready(mut l) => l.flush(),
            Async::NotReady => Err(would_block()),
        }
    }
}

impl AsyncWrite for AsyncPtyMasterWriteHalf {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        let mut l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        l.shutdown()
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error>
    where
        Self: Sized,
    {
        let mut l = try_ready!(wrap_as_io(self.handle.poll_lock()));
        l.write_buf(buf)
    }
}

fn wrap_as_io<T>(t: Async<T>) -> Result<Async<T>, io::Error> {
    Ok(t)
}
