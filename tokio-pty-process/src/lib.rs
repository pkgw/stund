//! Hacked up pty-process fun.

extern crate futures;
extern crate libc;
extern crate mio;
extern crate tokio_core; // TODO: migrate to just `tokio`; blocking on `tokio_signal`.
extern crate tokio_io;
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
use tokio_core::reactor::{Handle, PollEvented};
use tokio_io::{AsyncWrite, AsyncRead, IoFuture};
use tokio_signal::unix::Signal;


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
        eprintln!("low-level reading from PTY");
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
        eprintln!("PTY REG: {:?} {:?} {:?}", token, interest, opts);
        EventedFd(&self.0.as_raw_fd()).register(poll,
                                                token,
                                                interest | UnixReady::hup(),
                                                opts)
    }

    fn reregister(&self, poll: &mio::Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        eprintln!("PTY REREG: {:?} {:?} {:?}", token, interest, opts);
        EventedFd(&self.0.as_raw_fd()).reregister(poll,
                                                  token,
                                                  interest | UnixReady::hup(),
                                                  opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        eprintln!("PTY DEREG");
        EventedFd(&self.0.as_raw_fd()).deregister(poll)
    }
}


pub struct AsyncPtyMaster(PollEvented<AsyncPtyFile>);

impl AsyncPtyMaster {
    pub fn open(handle: &Handle) -> Result<Self, io::Error> {
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

        Ok(AsyncPtyMaster(PollEvented::new(AsyncPtyFile::new(inner), handle)?))
    }

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
    pub fn new(inner: process::Child, handle: &Handle) -> Child {
        Child {
            inner: inner,
            kill_on_drop: true,
            reaped: false,
            sigchld: Signal::new(libc::SIGCHLD, handle).flatten_stream(),
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


// Now, the extension to the Command type to glue it all together.

pub trait CommandExt {
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster, handle: &Handle) -> io::Result<Child>;
}


impl CommandExt for process::Command {
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster, handle: &Handle) -> io::Result<Child> {
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

        Ok(Child::new(self.spawn()?, handle))
    }
}
