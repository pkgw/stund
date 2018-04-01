// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! PTY stuff.
//!
//! Procedure based on http://rachid.koucha.free.fr/tech_corner/pty_pdip.html.

extern crate libc;

use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process;


pub fn create() -> Result<File, io::Error> {
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::grantpt(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::unlockpt(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { File::from_raw_fd(fd) })
}


pub trait FilePtyExt {
    fn open_pty_slave(&self) -> Result<File, io::Error>;
}

impl FilePtyExt for File {
    fn open_pty_slave(&self) -> Result<File, io::Error> {
        let mut buf: [libc::c_char; 512] = [0; 512];
        let fd = self.as_raw_fd();

        if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let ptsname = OsStr::from_bytes(unsafe { CStr::from_ptr(&buf as _) }.to_bytes());
        OpenOptions::new().read(true).write(true).open(ptsname)
    }
}



pub trait CommandPtyExt {
    fn slave_to_pty(&mut self, master: &File) -> Result<&mut Self, io::Error>;
}

impl CommandPtyExt for process::Command {
    fn slave_to_pty(&mut self, master: &File) -> Result<&mut Self, io::Error> {
        let master_fd = master.as_raw_fd();
        let slave = master.open_pty_slave()?;
        let slave_fd = slave.as_raw_fd();

        self.stdin(slave.try_clone()?);
        self.stdout(slave.try_clone()?);
        self.stderr(slave);

        // XXX any need to close slave handles in the parent process beyond
        // what's done here?

        self.before_exec(move || {
            let mut attrs: libc::termios = unsafe { mem::zeroed() };

            if unsafe { libc::tcgetattr(slave_fd, &mut attrs as _) } != 0 {
                return Err(io::Error::last_os_error());
            }

            unsafe { libc::cfmakeraw(&mut attrs as _) };

            if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &attrs as _) } != 0 {
                return Err(io::Error::last_os_error());
            }

            // This is OK even though we don't own master since this process is
            // about to become something totally different anyway.
            if unsafe { libc::close(master_fd) } != 0 {
                return Err(io::Error::last_os_error());
            }

            if unsafe { libc::setsid() } < 0 {
                return Err(io::Error::last_os_error());
            }

            if unsafe { libc::ioctl(0, libc::TIOCSCTTY, 1) } != 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        });

        Ok(self)
    }
}
