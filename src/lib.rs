// Copyright (C) 2014-2015 Mickaël Salaün
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Lesser General Public License as published by
// the Free Software Foundation, version 3 of the License.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

#![feature(process_exec)]

extern crate fd;
extern crate libc;
extern crate termios;

use fd::{Pipe, set_flags, splice_loop, unset_append_flag};
use ffi::{get_winsize, openpty};
use libc::c_int;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use termios::{Termios, tcsetattr};

pub use fd::FileDesc;

pub mod ffi;

pub struct TtyServer {
    master: File,
    slave: Option<File>,
    path: PathBuf,
}

pub struct TtyClient {
    // Need to keep the master file descriptor open
    #[allow(dead_code)]
    master: FileDesc,
    master_status: Option<c_int>,
    peer: FileDesc,
    peer_status: Option<c_int>,
    termios_orig: Termios,
    do_flush: Arc<AtomicBool>,
    flush_event: Receiver<()>,
}

impl TtyServer {
    /// Create a new TTY with the same configuration (termios and size) as the `template` TTY
    pub fn new<T>(template: Option<&T>) -> io::Result<TtyServer> where T: AsRawFd {
        // Native runtime does not support RtioTTY::get_winsize()
        let pty = match template {
            Some(t) => try!(openpty(Some(&try!(Termios::from_fd(t.as_raw_fd()))), Some(&try!(get_winsize(t))))),
            None => try!(openpty(None, None)),
        };

        Ok(TtyServer {
            master: pty.master,
            slave: Some(pty.slave),
            path: pty.path,
        })
    }

    /// Bind the peer TTY with the server TTY
    pub fn new_client<T>(&self, peer: T) -> io::Result<TtyClient> where T: AsRawFd + IntoRawFd {
        let master = FileDesc::new(self.master.as_raw_fd(), false);
        TtyClient::new(master, peer)
    }

    /// Get the TTY master file descriptor usable by a `TtyClient`
    pub fn get_master(&self) -> &File {
        &self.master
    }

    /// Take the TTY slave file descriptor to manually pass it to a process
    pub fn take_slave(&mut self) -> Option<File> {
        self.slave.take()
    }

    /// Spawn a new process connected to the slave TTY
    pub fn spawn(&mut self, mut cmd: Command) -> io::Result<Child> {
        match self.slave.take() {
            Some(slave) => {
                // Force new session
                // TODO: tcsetpgrp
                cmd.stdin(unsafe { Stdio::from_raw_fd(slave.as_raw_fd()) }).
                    stdout(unsafe { Stdio::from_raw_fd(slave.as_raw_fd()) }).
                    // Must close the slave FD to not wait indefinitely the end of the proxy
                    stderr(unsafe { Stdio::from_raw_fd(slave.into_raw_fd()) }).
                    before_exec(|| { let _ = unsafe { libc::setsid() }; Ok(()) }).
                    spawn()
            },
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "No TTY slave")),
        }
    }
}

impl AsRef<Path> for TtyServer {
    /// Get the server TTY path
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

// TODO: Handle SIGWINCH to dynamically update WinSize
// TODO: Replace `spawn` with `scoped` and share variables
impl TtyClient {
    /// Setup the peer TTY client (e.g. stdio) and bind it to the master TTY server
    pub fn new<T, U>(master: T, peer: U) -> io::Result<TtyClient>
            where T: AsRawFd + IntoRawFd, U: AsRawFd + IntoRawFd {
        // Setup peer terminal configuration
        let termios_orig = try!(Termios::from_fd(peer.as_raw_fd()));
        let mut termios_peer = try!(Termios::from_fd(peer.as_raw_fd()));
        termios_peer.c_lflag &= !(termios::ECHO | termios::ICANON | termios::ISIG);
        termios_peer.c_iflag &= !(termios::IGNBRK | termios::ICRNL);
        termios_peer.c_iflag |= termios::BRKINT;
        termios_peer.c_cc[termios::VMIN] = 1;
        termios_peer.c_cc[termios::VTIME] = 0;
        // XXX: cfmakeraw
        try!(tcsetattr(peer.as_raw_fd(), termios::TCSAFLUSH, &termios_peer));

        // Create the proxy
        let do_flush_main = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx): (Sender<()>, Receiver<()>) = channel();

        // Master to peer
        let (m2p_tx, m2p_rx) = match Pipe::new() {
            Ok(p) => (p.writer, p.reader),
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
        };
        let do_flush = do_flush_main.clone();
        let master_fd = master.as_raw_fd();
        thread::spawn(move || splice_loop(do_flush, None, master_fd, m2p_tx.as_raw_fd()));

        let do_flush = do_flush_main.clone();
        let peer_fd = peer.as_raw_fd();
        let peer_status = try!(unset_append_flag(peer_fd));
        thread::spawn(move || splice_loop(do_flush, None, m2p_rx.as_raw_fd(), peer_fd));

        // Peer to master
        let (p2m_tx, p2m_rx) = match Pipe::new() {
            Ok(p) => (p.writer, p.reader),
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
        };
        let do_flush = do_flush_main.clone();
        let peer_fd = peer.as_raw_fd();
        thread::spawn(move || splice_loop(do_flush, None, peer_fd, p2m_tx.as_raw_fd()));

        let do_flush = do_flush_main.clone();
        let master_fd = master.as_raw_fd();
        let master_status = try!(unset_append_flag(master_fd));
        thread::spawn(move || splice_loop(do_flush, Some(event_tx), p2m_rx.as_raw_fd(), master_fd));

        Ok(TtyClient {
            master: FileDesc::new(master.into_raw_fd(), true),
            master_status: master_status,
            peer: FileDesc::new(peer.into_raw_fd(), true),
            peer_status: peer_status,
            termios_orig: termios_orig,
            do_flush: do_flush_main,
            flush_event: event_rx,
        })
    }

    /// Wait until the TTY binding broke (e.g. the connected process exited)
    pub fn wait(&self) {
        while !self.do_flush.load(Relaxed) {
            let _ = self.flush_event.recv();
        }
    }
}

impl Drop for TtyClient {
    /// Cleanup the peer TTY
    fn drop(&mut self) {
        self.do_flush.store(true, Relaxed);
        let _ = tcsetattr(self.peer.as_raw_fd(), termios::TCSAFLUSH, &self.termios_orig);

        // Restore the append flag if needed
        let tty_fd = [(&self.peer, self.peer_status), (&self.master, self.master_status)];
        for &(fd, status) in tty_fd.iter() {
            if let Some(s) = status {
                let _ = set_flags(fd.as_raw_fd(), s);
            }
        }
    }
}
