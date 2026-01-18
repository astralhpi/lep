//! PTY handling for lep
//!
//! Spawns a child process in a PTY and provides read/write access.

use nix::libc;
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, setsid, ForkResult, Pid};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

/// A PTY master handle for communicating with a child process
pub struct PtyMaster {
    master: File,
    /// Child process ID for cleanup
    child_pid: Pid,
}

impl PtyMaster {
    /// Spawn a command in a new PTY
    ///
    /// Returns a PtyMaster that can be used to read/write to the child process
    pub fn spawn(args: &[&str]) -> io::Result<Self> {
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "args cannot be empty",
            ));
        }

        // Open PTY pair
        let OpenptyResult { master, slave } = openpty(None, None).map_err(io::Error::other)?;

        // Fork
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                // Child process
                drop(master); // Close master in child

                // Create new session
                setsid().ok();

                // Set up slave as controlling terminal using libc directly
                let slave_fd = slave.as_raw_fd();
                unsafe {
                    libc::dup2(slave_fd, 0); // stdin
                    libc::dup2(slave_fd, 1); // stdout
                    libc::dup2(slave_fd, 2); // stderr
                }

                drop(slave); // Close original slave fd

                // Execute command
                let c_args: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();

                execvp(&c_args[0], &c_args).ok();
                std::process::exit(1);
            }
            Ok(ForkResult::Parent { child }) => {
                // Parent process
                drop(slave); // Close slave in parent

                let master_fd = master.into_raw_fd();
                let master_file = unsafe { File::from_raw_fd(master_fd) };

                Ok(PtyMaster {
                    master: master_file,
                    child_pid: child,
                })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    /// Get the raw file descriptor for polling
    pub fn as_raw_fd(&self) -> i32 {
        self.master.as_raw_fd()
    }

    /// Set the PTY window size
    pub fn set_winsize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Read for PtyMaster {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.master.read(buf)
    }
}

impl Write for PtyMaster {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.master.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.master.flush()
    }
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        // Try to reap the child process to prevent zombies
        // First, try non-blocking wait
        if let Ok(WaitStatus::StillAlive) = waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
            // Child is still running, send SIGHUP (like closing terminal)
            let _ = kill(self.child_pid, Signal::SIGHUP);

            // Give it a moment to exit gracefully
            std::thread::sleep(std::time::Duration::from_millis(10));

            // Try to reap again
            if let Ok(WaitStatus::StillAlive) = waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG))
            {
                // Still alive, force kill
                let _ = kill(self.child_pid, Signal::SIGKILL);
                // Final reap
                let _ = waitpid(self.child_pid, None);
            }
        }
    }
}
