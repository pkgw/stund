// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under both the MIT License and the Apache-2.0 license.

#![deny(missing_docs)]

//! Spawn a child process under a pseudo-TTY, interacting with it
//! asynchronously using Tokio.
//!
//! A [pseudo-terminal](https://en.wikipedia.org/wiki/Pseudoterminal) (or
//! “pseudo-TTY” or “PTY”) is a special Unix file handle that models the kind
//! of text terminal through which users used to interact with computers. A
//! PTY enables a specialized form of bidirectional interprocess communication
//! that a variety of user-facing Unix programs take advantage of.
//!
//! The basic way to use this crate is:
//!
//! 1. Create a Tokio [Reactor](https://docs.rs/tokio/*/tokio/reactor/struct.Reactor.html)
//!    that will handle all of your asynchronous I/O.
//! 2. Create an `AsyncPtyMaster` that represents your ownership of
//!    an OS pseudo-terminal.
//! 3. Use your master and the `spawn_pty_async` function of the `CommandExt`
//!    extension trait, which extends `std::process::Command`, to launch a child
//!    process that is connected to your master.
//! 4. Optionally control the child process (e.g. send it signals) through the
//!    `Child` value returned by that function.
//!
//! This crate only works on Unix since pseudo-terminals are a Unix-specific
//! concept.
//!
//! The `Child` type is largely copied from Alex Crichton’s
//! [tokio-process](https://github.com/alexcrichton/tokio-process) crate.

extern crate futures;
extern crate libc;
extern crate mio;
extern crate tokio;
extern crate tokio_signal;

use futures::{Async, Future, Poll, Stream};
use futures::future::FlattenStream;
use libc::c_int;
use mio::unix::{EventedFd, UnixReady};
use mio::{PollOpt, Ready, Token};
use mio::event::Evented;
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::prelude::*;
use std::os::unix::process::CommandExt as StdUnixCommandExt;
use std::process::{self, ExitStatus};
use tokio::io::{AsyncWrite, AsyncRead};
use tokio_signal::unix::Signal;
use tokio_signal::IoFuture;
use tokio::reactor::{PollEvented2};


// First set of hoops to jump through: a read-write pseudo-terminal master
// with full async support. As far as I can tell, we need to create an inner
// wrapper type to implement Evented on a type that we can then wrap in a
// PollEvented. Lame.

#[derive(Debug)]
struct AsyncPtyFile(File);

impl AsyncPtyFile {
    pub fn new(inner: File) -> Self {
        AsyncPtyFile(inner)
    }
}

impl Read for AsyncPtyFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl Write for AsyncPtyFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Evented for AsyncPtyFile {
    fn register(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).register(poll,
                                                token,
                                                interest | UnixReady::hup(),
                                                opts)
    }

    fn reregister(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).reregister(poll,
                                                  token,
                                                  interest | UnixReady::hup(),
                                                  opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).deregister(poll)
    }
}


/// A handle to a pseudo-TTY master that can be interacted with
/// asynchronously.
///
/// This type implements both `AsyncRead` and `AsyncWrite`.
pub struct AsyncPtyMaster(PollEvented2<AsyncPtyFile>);

impl AsyncPtyMaster {
    /// Open a pseudo-TTY master.
    ///
    /// This function performs the C library calls `posix_openpt()`,
    /// `grantpt()`, and `unlockpt()`. It also sets the resulting pseudo-TTY
    /// master handle to nonblocking mode.
    pub fn open() -> Result<Self, io::Error> {
        let inner = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::grantpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::unlockpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }

            // XXX just set nonblock on open? will that make grantpt and unlockpt
            // start acting funky?

            let r = libc::fcntl(fd, libc::F_GETFL);
            if r < 0 {
                return Err(io::Error::last_os_error())
            }

            if libc::fcntl(fd, libc::F_SETFL, r | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error())
            }

            File::from_raw_fd(fd)
        };

        Ok(AsyncPtyMaster(PollEvented2::new(AsyncPtyFile::new(inner))))
    }

    /// Open a pseudo-TTY slave that is connected to this master.
    ///
    /// The resulting file handle is *not* set to non-blocking mode.
    fn open_sync_pty_slave(&self) -> Result<File, io::Error> {
        let mut buf: [libc::c_char; 512] = [0; 512];
        let fd = self.as_raw_fd();

        if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let ptsname = OsStr::from_bytes(unsafe { CStr::from_ptr(&buf as _) }.to_bytes());
        OpenOptions::new().read(true).write(true).open(ptsname)
    }
}

impl AsRawFd for AsyncPtyMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().0.as_raw_fd()
    }
}

impl Read for AsyncPtyMaster {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl AsyncRead for AsyncPtyMaster {
}

impl Write for AsyncPtyMaster {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AsyncWrite for AsyncPtyMaster {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.0.shutdown()
    }
}


// Now, the async-ified child process framework.

/// A child process that can be interacted with through a pseudo-TTY.
#[must_use = "futures do nothing unless polled"]
pub struct Child {
    inner: process::Child,
    kill_on_drop: bool,
    reaped: bool,
    sigchld: FlattenStream<IoFuture<Signal>>,
}

impl fmt::Debug for Child {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("pid", &self.inner.id())
            .field("inner", &self.inner)
            .field("kill_on_drop", &self.kill_on_drop)
            .field("reaped", &self.reaped)
            .field("sigchld", &"..")
            .finish()
    }
}

impl Child {
    fn new(inner: process::Child) -> Child {
        Child {
            inner: inner,
            kill_on_drop: true,
            reaped: false,
            sigchld: Signal::new(libc::SIGCHLD).flatten_stream(),
        }
    }

    /// Returns the OS-assigned process identifier associated with this child.
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    /// Forces the child to exit.
    ///
    /// This is equivalent to sending a SIGKILL on unix platforms.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.reaped {
            Ok(())
        } else {
            self.inner.kill()
        }
    }

    /// Drop this `Child` without killing the underlying process.
    ///
    /// Normally a `Child` is killed if it's still alive when dropped, but this
    /// method will ensure that the child may continue running once the `Child`
    /// instance is dropped.
    pub fn forget(mut self) {
        self.kill_on_drop = false;
    }

    /// Check whether this `Child` has exited yet.
    pub fn poll_exit(&mut self) -> Poll<ExitStatus, io::Error> {
        assert!(!self.reaped);

        loop {
            if let Some(e) = self.try_wait()? {
                self.reaped = true;
                return Ok(e.into())
            }

            // If the child hasn't exited yet, then it's our responsibility to
            // ensure the current task gets notified when it might be able to
            // make progress.
            //
            // As described in `spawn` above, we just indicate that we can
            // next make progress once a SIGCHLD is received.
            if self.sigchld.poll()?.is_not_ready() {
                return Ok(Async::NotReady)
            }
        }
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let id = self.id() as c_int;
        let mut status = 0;

        loop {
            match unsafe { libc::waitpid(id, &mut status, libc::WNOHANG) } {
                0 => return Ok(None),

                n if n < 0 => {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue
                    }
                    return Err(err)
                },

                n => {
                    assert_eq!(n, id);
                    return Ok(Some(ExitStatus::from_raw(status)))
                },
            }
        }
    }
}


impl Future for Child {
    type Item = ExitStatus;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<ExitStatus, io::Error> {
        self.poll_exit()
    }
}


impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            drop(self.kill());
        }
    }
}


/// An extension trait for the `std::process::Command` type.
///
/// This trait provides a new `spawn_pty_async` method that allows one to
/// spawn a new process that is connected to the current process through a
/// pseudo-TTY.
pub trait CommandExt {
    /// Spawn a subprocess that connects to the current one through a
    /// pseudo-TTY.
    ///
    /// This function creates the necessary PTY slave and uses
    /// `std::process::Command::before_exec` to do the neccessary setup before
    /// the child process is spawned. In particular, it sets the slave PTY
    /// handle to raw mode and calls `setsid()` to launch a new TTY sesson.
    ///
    /// The child process’s standard input, standard output, and standard
    /// error are all connected to the pseudo-TTY slave.
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child>;

    /// Spawn a subprocess that connects to the current one through a
    /// pseudo-TTY.
    ///
    /// This function creates the necessary PTY slave and uses
    /// `std::process::Command::before_exec` to do the neccessary setup before
    /// the child process is spawned. In particular, it calls `setsid()` to
    /// launch a new TTY sesson.
    ///
    /// Unlike spawn_pty_async, it does not set the child tty to raw mode.
    ///
    /// The child process’s standard input, standard output, and standard
    /// error are all connected to the pseudo-TTY slave.
    fn spawn_pty_async_pristine(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child>;
}


impl CommandExt for process::Command {
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child> {
        let master_fd = ptymaster.as_raw_fd();
        let slave = ptymaster.open_sync_pty_slave()?;
        let slave_fd = slave.as_raw_fd();

        self.stdin(slave.try_clone()?);
        self.stdout(slave.try_clone()?);
        self.stderr(slave);

        // XXX any need to close slave handles in the parent process beyond
        // what's done here?

        self.before_exec(move || {
            unsafe {
                let mut attrs: libc::termios = mem::zeroed();

                if libc::tcgetattr(slave_fd, &mut attrs as _) != 0 {
                    return Err(io::Error::last_os_error());
                }

                libc::cfmakeraw(&mut attrs as _);

                if libc::tcsetattr(slave_fd, libc::TCSANOW, &attrs as _) != 0 {
                    return Err(io::Error::last_os_error());
                }

                // This is OK even though we don't own master since this process is
                // about to become something totally different anyway.
                if libc::close(master_fd) != 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::ioctl(0, libc::TIOCSCTTY, 1) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        });

        Ok(Child::new(self.spawn()?))
    }

    fn spawn_pty_async_pristine(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child> {
        let master_fd = ptymaster.as_raw_fd();
        let slave = ptymaster.open_sync_pty_slave()?;

        self.stdin(slave.try_clone()?);
        self.stdout(slave.try_clone()?);
        self.stderr(slave);

        // XXX any need to close slave handles in the parent process beyond
        // what's done here?

        self.before_exec(move || {
            unsafe {
                // This is OK even though we don't own master since this process is
                // about to become something totally different anyway.
                if libc::close(master_fd) != 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::ioctl(0, libc::TIOCSCTTY, 1) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        });

        Ok(Child::new(self.spawn()?))
    }
}
