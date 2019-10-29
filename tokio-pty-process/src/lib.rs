// Copyright 2018-2019 Peter Williams <peter@newton.cx>
// Licensed under both the MIT License and the Apache-2.0 license.

#![deny(missing_docs)]
#![doc(html_root_url = "https://docs.rs/tokio-pty-process/0.4.0")]

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
//! 3. Use your master and the `spawn_pty_async` or `spawn_pty_async_raw`
//!    functions of the `CommandExt` extension trait, which extends
//!    `std::process::Command`, to launch a child process that is connected to
//!    your master.
//! 4. Optionally control the child process (e.g. send it signals) through the
//!    `Child` value returned by that function.
//!
//! This crate only works on Unix since pseudo-terminals are a Unix-specific
//! concept.
//!
//! The `Child` type is largely copied from Alex Crichton’s
//! [tokio-process](https://github.com/alexcrichton/tokio-process) crate.

use futures::future::FlattenStream;
use futures::{try_ready, Async, Future, Poll, Stream};
use libc::{c_int, c_ushort};
use mio::event::Evented;
use mio::unix::{EventedFd, UnixReady};
use mio::{PollOpt, Ready, Token};
use std::ffi::{CStr, OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::prelude::*;
use std::os::unix::process::CommandExt as StdUnixCommandExt;
use std::process::{self, ExitStatus};
use std::sync::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::reactor::PollEvented2;
use tokio_signal::unix::Signal;
use tokio_signal::IoFuture;
use shared_child::SharedChild;

mod split;
pub use split::{AsyncPtyMasterReadHalf, AsyncPtyMasterWriteHalf};

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
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).register(poll, token, interest | UnixReady::hup(), opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).reregister(poll, token, interest | UnixReady::hup(), opts)
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
            // On MacOS, O_NONBLOCK is not documented as an allowed option to
            // posix_openpt(), but it is in fact allowed and functional, and
            // trying to add it later with fcntl() is forbidden. Meanwhile, on
            // FreeBSD, O_NONBLOCK is *not* an allowed option to
            // posix_openpt(), and the only way to get a nonblocking PTY
            // master is to add the nonblocking flag with fcntl() later. So,
            // we have to jump through some #[cfg()] hoops.

            const APPLY_NONBLOCK_AFTER_OPEN: bool = cfg!(target_os = "freebsd");

            let fd = if APPLY_NONBLOCK_AFTER_OPEN {
                libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY)
            } else {
                libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK)
            };

            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::grantpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::unlockpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }

            if APPLY_NONBLOCK_AFTER_OPEN {
                let flags = libc::fcntl(fd, libc::F_GETFL, 0);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }

            File::from_raw_fd(fd)
        };

        Ok(AsyncPtyMaster(PollEvented2::new(AsyncPtyFile::new(inner))))
    }

    /// Split the AsyncPtyMaster into an AsyncPtyReadHalf implementing `Read` and
    /// and `AsyncRead` as well as an `AsyncPtyWriteHalf` implementing
    /// `AsyncPtyWrite`.
    pub fn split(self) -> (AsyncPtyMasterReadHalf, AsyncPtyMasterWriteHalf) {
        split::split(self)
    }

    /// Open a pseudo-TTY slave that is connected to this master.
    ///
    /// The resulting file handle is *not* set to non-blocking mode.
    fn open_sync_pty_slave(&self) -> Result<File, io::Error> {
        let mut buf: [libc::c_char; 512] = [0; 512];
        let fd = self.as_raw_fd();

        #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
        {
            if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        unsafe {
            let st = libc::ptsname(fd);
            if st.is_null() {
                return Err(io::Error::last_os_error());
            }
            libc::strncpy(buf.as_mut_ptr(), st, buf.len());
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

impl AsyncRead for AsyncPtyMaster {}

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
    inner: SharedChild,
    kill_on_drop: bool,
    sigchld: Mutex<FlattenStream<IoFuture<Signal>>>,
}

impl fmt::Debug for Child {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("pid", &self.inner.id())
            .field("inner", &self.inner)
            .field("kill_on_drop", &self.kill_on_drop)
            .field("sigchld", &"..")
            .finish()
    }
}

impl Child {
    fn new(inner: SharedChild) -> Child {
        Child {
            inner,
            kill_on_drop: true,
            sigchld: Mutex::new(Signal::new(libc::SIGCHLD).flatten_stream()),
        }
    }

    /// Returns the OS-assigned process identifier associated with this child.
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    /// Forces the child to exit.
    ///
    /// This is equivalent to sending a SIGKILL on unix platforms.
    pub fn kill(&self) -> io::Result<()> {
        self.inner.kill()
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
    ///
    /// This is equivalent to `child.kill()` but does not require a mutable reference.
    pub fn poll_exit(&self) -> Poll<ExitStatus, io::Error> {

        loop {
            if let Some(e) = self.try_wait()? {
                return Ok(e.into());
            }

            // If the child hasn't exited yet, then it's our responsibility to
            // ensure the current task gets notified when it might be able to
            // make progress.
            //
            // As described in `spawn` above, we just indicate that we can
            // next make progress once a SIGCHLD is received.
            if self.sigchld.lock().unwrap().poll()?.is_not_ready() {
                return Ok(Async::NotReady);
            }
        }
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
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

/// A Future for getting the Pty file descriptor.
///
/// # Example
///
/// ```
/// extern crate tokio;
/// extern crate tokio_pty_process;
///
/// use tokio_pty_process::{AsyncPtyMaster, AsyncPtyFd};
/// use tokio::prelude::*;
///
/// fn main() {
///     let master = AsyncPtyMaster::open()
///         .expect("Could not open the PTY");
///
///     let fd = AsyncPtyFd::from(master).wait()
///         .expect("Could not get the File descriptor");
/// }
/// ```
pub struct AsyncPtyFd<T: AsAsyncPtyFd>(T);

impl<T: AsAsyncPtyFd> AsyncPtyFd<T> {
    /// Construct a new AsyncPtyFd future
    pub fn from(inner: T) -> Self {
        AsyncPtyFd(inner)
    }
}

impl<T: AsAsyncPtyFd> Future for AsyncPtyFd<T> {
    type Item = RawFd;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<RawFd, io::Error> {
        self.0.as_async_pty_fd()
    }
}

/// Trait to asynchronously get the `RawFd` of the master side of the PTY
pub trait AsAsyncPtyFd {
    /// Return a `Poll` containing the RawFd
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error>;
}

impl AsAsyncPtyFd for AsyncPtyMaster {
    fn as_async_pty_fd(&self) -> Poll<RawFd, io::Error> {
        Ok(Async::Ready(self.as_raw_fd()))
    }
}

/// Trait containing generalized methods for PTYs
pub trait PtyMaster {
    /// Return the full pathname of the slave device counterpart
    ///
    /// # Example
    ///
    /// ```
    /// extern crate tokio;
    /// extern crate tokio_pty_process;
    ///
    /// use std::ffi::OsString;
    /// use tokio::prelude::*;
    /// use tokio_pty_process::{AsyncPtyMaster, PtyMaster};
    ///
    /// struct PtsName<T: PtyMaster>(T);
    ///
    /// impl<T: PtyMaster> Future for PtsName<T> {
    ///     type Item = OsString;
    ///     type Error = std::io::Error;
    ///
    ///     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    ///         self.0.ptsname()
    ///     }
    /// }
    ///
    /// fn main() {
    ///     let master = AsyncPtyMaster::open().expect("Could not open the PTY");
    ///
    ///     let ptsname = PtsName(master).wait().expect("Could not get the ptsname");
    ///
    ///     println!("PTS name: {}", ptsname.to_string_lossy());
    /// }
    /// ```
    fn ptsname(&self) -> Poll<OsString, io::Error>;

    /// Resize the PTY
    ///
    /// # Example
    ///
    /// ```
    /// extern crate tokio;
    /// extern crate tokio_pty_process;
    /// extern crate libc;
    ///
    /// use tokio_pty_process::{AsyncPtyMaster, PtyMaster, CommandExt};
    /// use tokio::prelude::*;
    /// use std::ffi::OsString;
    /// use libc::c_ushort;
    /// struct Resize<T: PtyMaster> {
    ///     pty: T,
    ///     rows: c_ushort,
    ///     cols: c_ushort,
    /// }
    ///
    /// impl<T: PtyMaster> Future for Resize<T> {
    ///     type Item = ();
    ///     type Error = std::io::Error;
    ///
    ///     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    ///         self.pty.resize(self.rows, self.cols)
    ///     }
    /// }
    ///
    /// fn main() {
    ///     let master = AsyncPtyMaster::open().expect("Could not open the PTY");
    ///
    ///     // On macos, it's only possible to resize a PTY with a child spawned
    ///     // On it, so let's just do that:
    ///     #[cfg(target_os="macos")]
    ///     let mut child = std::process::Command::new("cat")
    ///         .spawn_pty_async(&master)
    ///         .expect("Could not spawn child");
    ///
    ///     Resize {
    ///         pty: master,
    ///         cols: 80,
    ///         rows: 50,
    ///     }
    ///     .wait()
    ///     .expect("Could not resize the PTY");
    ///
    ///     #[cfg(target_os="macos")]
    ///     child.kill().expect("Could not kill child");
    /// }
    /// ```
    fn resize(&self, rows: c_ushort, cols: c_ushort) -> Poll<(), io::Error>;

    /// Get the PTY size
    ///
    /// # Example
    ///
    /// ```
    /// extern crate tokio;
    /// extern crate tokio_pty_process;
    /// extern crate libc;
    ///
    /// use tokio_pty_process::{AsyncPtyMaster, PtyMaster, CommandExt};
    /// use tokio::prelude::*;
    /// use std::ffi::OsString;
    /// use libc::c_ushort;
    ///
    /// struct GetSize<'a, T: PtyMaster> (&'a T);
    /// impl<'a, T: PtyMaster> Future for GetSize<'a, T> {
    ///     type Item = (c_ushort, c_ushort);
    ///     type Error = std::io::Error;
    ///     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    ///         self.0.winsize()
    ///     }
    /// }
    ///
    /// fn main() {
    ///     let master = AsyncPtyMaster::open().expect("Could not open the PTY");
    ///
    ///     // On macos, it's only possible to resize a PTY with a child spawned
    ///     // On it, so let's just do that:
    ///     #[cfg(target_os="macos")]
    ///     let mut child = std::process::Command::new("cat")
    ///         .spawn_pty_async(&master)
    ///         .expect("Could not spawn child");
    ///
    ///     let (rows, cols) = GetSize(&master)
    ///         .wait()
    ///         .expect("Could not get PTY size");
    ///
    ///     #[cfg(target_os="macos")]
    ///     child.kill().expect("Could not kill child");
    /// }
    /// ```
    fn winsize(&self) -> Poll<(c_ushort, c_ushort), io::Error>;
}

impl<T: AsAsyncPtyFd> PtyMaster for T {
    fn ptsname(&self) -> Poll<OsString, io::Error> {
        let mut buf: [libc::c_char; 512] = [0; 512];
        let fd = try_ready!(self.as_async_pty_fd());

        #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
        {
            if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        unsafe {
            let st = libc::ptsname(fd);
            if st.is_null() {
                return Err(io::Error::last_os_error());
            }
            libc::strncpy(buf.as_mut_ptr(), st, buf.len());
        }
        let ptsname = OsStr::from_bytes(unsafe { CStr::from_ptr(&buf as _) }.to_bytes());
        Ok(Async::Ready(ptsname.to_os_string()))
    }

    fn winsize(&self) -> Poll<(c_ushort, c_ushort), io::Error> {
        let fd = try_ready!(self.as_async_pty_fd());
        let mut winsz: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ.into(), &mut winsz) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Async::Ready((winsz.ws_row, winsz.ws_col)))
    }

    fn resize(&self, rows: c_ushort, cols: c_ushort) -> Poll<(), io::Error> {
        let fd = try_ready!(self.as_async_pty_fd());
        let winsz = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ.into(), &winsz) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Async::Ready(()))
    }
}

/// A private trait for the extending `std::process::Command`.
trait CommandExtInternal {
    fn spawn_pty_async_full(&mut self, ptymaster: &AsyncPtyMaster, raw: bool) -> io::Result<Child>;
}

impl CommandExtInternal for process::Command {
    fn spawn_pty_async_full(&mut self, ptymaster: &AsyncPtyMaster, raw: bool) -> io::Result<Child> {
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
                if raw {
                    let mut attrs: libc::termios = mem::zeroed();

                    if libc::tcgetattr(slave_fd, &mut attrs as _) != 0 {
                        return Err(io::Error::last_os_error());
                    }

                    libc::cfmakeraw(&mut attrs as _);

                    if libc::tcsetattr(slave_fd, libc::TCSANOW, &attrs as _) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }

                // This is OK even though we don't own master since this process is
                // about to become something totally different anyway.
                if libc::close(master_fd) != 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }

                if libc::ioctl(0, libc::TIOCSCTTY.into(), 1) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        });

        Ok(Child::new(SharedChild::spawn(self)?))
    }
}

/// An extension trait for the `std::process::Command` type.
///
/// This trait provides new `spawn_pty_async` and `spawn_pty_async_raw`
/// methods that allow one to spawn a new process that is connected to the
/// current process through a pseudo-TTY.
pub trait CommandExt {
    /// Spawn a subprocess that connects to the current one through a
    /// pseudo-TTY in canonical (“cooked“, not “raw”) mode.
    ///
    /// This function creates the necessary PTY slave and uses
    /// `std::process::Command::before_exec` to do the neccessary setup before
    /// the child process is spawned. In particular, it calls `setsid()` to
    /// launch a new TTY sesson.
    ///
    /// The child process’s standard input, standard output, and standard
    /// error are all connected to the pseudo-TTY slave.
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child>;

    /// Spawn a subprocess that connects to the current one through a
    /// pseudo-TTY in raw (“non-canonical”, not “cooked”) mode.
    ///
    /// This function creates the necessary PTY slave and uses
    /// `std::process::Command::before_exec` to do the neccessary setup before
    /// the child process is spawned. In particular, it sets the slave PTY
    /// handle to raw mode and calls `setsid()` to launch a new TTY sesson.
    ///
    /// The child process’s standard input, standard output, and standard
    /// error are all connected to the pseudo-TTY slave.
    fn spawn_pty_async_raw(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child>;
}

impl CommandExt for process::Command {
    fn spawn_pty_async(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child> {
        self.spawn_pty_async_full(ptymaster, false)
    }

    fn spawn_pty_async_raw(&mut self, ptymaster: &AsyncPtyMaster) -> io::Result<Child> {
        self.spawn_pty_async_full(ptymaster, true)
    }
}

#[cfg(test)]
mod tests {
    extern crate errno;
    extern crate libc;

    use super::*;

    /// Test that the PTY master file descriptor is in nonblocking mode. We do
    /// this in a pretty hacky and dumb way, by creating the AsyncPtyMaster
    /// and then just snarfing its FD and seeing whether a Unix `read(2)` call
    /// errors out with EWOULDBLOCK (instead of blocking forever). In
    /// principle it would be nice to actually spawn a subprogram and test
    /// reading through the whole Tokio I/O subsystem, but that's annoying to
    /// implement and can actually muddy the picture. Namely: if you try to
    /// `master.read()` inside a Tokio event loop here, on Linux you'll get an
    /// ErrorKind::WouldBlock I/O error from Tokio without it even attempting
    /// the underlying `read(2)` system call, because Tokio uses epoll to test
    /// the FD's readiness in a way that works orthogonal to whether it's set
    /// to non-blocking mode.
    #[test]
    fn basic_nonblocking() {
        let master = AsyncPtyMaster::open().unwrap();

        let fd = master.as_raw_fd();
        let mut buf = [0u8; 128];
        let rval = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 128) };
        let errno: i32 = errno::errno().into();

        assert_eq!(rval, -1);
        assert_eq!(errno, libc::EWOULDBLOCK as i32);
    }

    struct GetSize<'a, T: PtyMaster>(&'a T);
    impl<'a, T: PtyMaster> Future for GetSize<'a, T> {
        type Item = (c_ushort, c_ushort);
        type Error = std::io::Error;
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.0.winsize()
        }
    }

    struct Resize<'a, T: PtyMaster> {
        pty: &'a T,
        rows: c_ushort,
        cols: c_ushort,
    }
    impl<'a, T: PtyMaster> Future for Resize<'a, T> {
        type Item = ();
        type Error = std::io::Error;
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.pty.resize(self.rows, self.cols)
        }
    }

    #[test]
    fn test_winsize() {
        let master = AsyncPtyMaster::open().expect("Could not open the PTY");

        // On macos, it's only possible to resize a PTY with a child spawned
        // On it, so let's just do that:
        #[cfg(target_os = "macos")]
        let mut child = std::process::Command::new("cat")
            .spawn_pty_async(&master)
            .expect("Could not spawn child");

        // Set the size
        Resize {
            pty: &master,
            cols: 80,
            rows: 50,
        }
        .wait()
        .expect("Could not resize the PTY");

        let (rows, cols) = GetSize(&master).wait().expect("Could not get PTY size");

        assert_eq!(cols, 80);
        assert_eq!(rows, 50);

        #[cfg(target_os = "macos")]
        child.kill().expect("Could not kill child");
    }
}
