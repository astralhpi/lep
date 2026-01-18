//! lep - Local Echo Proxy
//!
//! Provides mosh-style predictive local echo over SSH without server-side installation.

use lep::command::Command;
use lep::emulator::{Emulator, VerifyResult};
use lep::pty::PtyMaster;
use log::{debug, info, trace};
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use nix::unistd::{pipe, write as nix_write};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use unicode_width::UnicodeWidthChar;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NAME: &str = env!("CARGO_PKG_NAME");

/// Prediction mode
#[derive(Debug, Clone, Copy, PartialEq)]
enum PredictMode {
    /// Always predict (ignore alternate screen detection)
    Always,
    /// Auto-detect (disable in alternate screen like vim/less) - default
    Auto,
    /// Never predict (passthrough mode)
    Never,
}

impl PredictMode {
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "always" => Ok(PredictMode::Always),
            "auto" => Ok(PredictMode::Auto),
            "never" | "none" | "off" => Ok(PredictMode::Never),
            _ => Err(format!(
                "Invalid predict mode '{}'. Use: always, auto, never",
                s
            )),
        }
    }
}

/// CLI configuration
struct Config {
    /// Command to run
    command: Vec<String>,
    /// Prediction mode
    predict_mode: PredictMode,
    /// Log file path (if any)
    log_file: Option<String>,
    /// Verbose level (0=off, 1=info, 2=debug, 3=trace)
    verbose: u8,
}

/// Global pipe for SIGWINCH notification (self-pipe trick)
static SIGWINCH_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// Signal handler for SIGWINCH
extern "C" fn sigwinch_handler(_: libc::c_int) {
    let fd = SIGWINCH_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        // Write a single byte to wake up the poll loop
        let _ = nix_write(unsafe { BorrowedFd::borrow_raw(fd) }, &[0u8]);
    }
}

/// Put terminal in raw mode and return the original termios settings
fn enter_raw_mode() -> io::Result<Termios> {
    let stdin = io::stdin();
    let original = termios::tcgetattr(&stdin).map_err(io::Error::other)?;

    let mut raw = original.clone();
    termios::cfmakeraw(&mut raw);

    termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(io::Error::other)?;

    Ok(original)
}

/// Restore terminal to original settings
fn restore_terminal(original: &Termios) {
    let stdin = io::stdin();
    let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, original);
}

/// Get terminal size (columns, rows)
fn get_terminal_size() -> (usize, usize) {
    use nix::pty::Winsize;
    use std::mem::MaybeUninit;

    // Try stdin first (more reliable when running under PTY)
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();

    let mut ws = MaybeUninit::<Winsize>::uninit();
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) };

    if result == 0 {
        let ws = unsafe { ws.assume_init() };
        if ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }

    // Fallback to stdout
    let stdout = io::stdout();
    let fd = stdout.as_raw_fd();
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) };

    if result == 0 {
        let ws = unsafe { ws.assume_init() };
        if ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }

    // Default
    (80, 24)
}

/// Set up SIGWINCH handler with self-pipe
fn setup_sigwinch() -> io::Result<(OwnedFd, OwnedFd)> {
    // Create pipe for signal notification
    let (read_fd, write_fd) = pipe().map_err(io::Error::other)?;

    // Store write end for signal handler
    SIGWINCH_PIPE_WRITE.store(write_fd.as_raw_fd(), Ordering::SeqCst);

    // Set up signal handler
    let sa = SigAction::new(
        SigHandler::Handler(sigwinch_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGWINCH, &sa).map_err(io::Error::other)?;
    }

    Ok((read_fd, write_fd))
}

/// Clean up SIGWINCH handler
fn cleanup_sigwinch(read_fd: OwnedFd, write_fd: OwnedFd) {
    SIGWINCH_PIPE_WRITE.store(-1, Ordering::SeqCst);
    // OwnedFd will close automatically when dropped
    drop(read_fd);
    drop(write_fd);
}

/// Run the proxy main loop
fn run_proxy(config: &Config) -> io::Result<()> {
    // Convert args to &str slice
    let args: Vec<&str> = config.command.iter().map(|s| s.as_str()).collect();

    debug!("Spawning command: {:?}", args);

    // Spawn child process in PTY
    let mut pty = PtyMaster::spawn(&args)?;

    // Set initial PTY size to match terminal
    let (cols, rows) = get_terminal_size();
    let _ = pty.set_winsize(cols as u16, rows as u16);

    // Set up SIGWINCH handler
    let (sigwinch_read, sigwinch_write) = setup_sigwinch()?;

    // Enter raw mode
    let original_termios = enter_raw_mode()?;

    // Enter alternate screen (clean slate, synced with emulator)
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[?1049h");
    let _ = stdout.flush();

    // Set up cleanup on exit
    let result = proxy_loop(&mut pty, sigwinch_read.as_raw_fd(), config.predict_mode);

    // Exit alternate screen (restore original content)
    let _ = stdout.write_all(b"\x1b[?1049l");
    let _ = stdout.flush();

    // Restore terminal
    restore_terminal(&original_termios);

    // Clean up signal handler
    cleanup_sigwinch(sigwinch_read, sigwinch_write);

    result
}

/// Input handler for creating commands from user input
struct InputHandler {
    /// UTF-8 buffer for multi-byte characters
    utf8_buf: Vec<u8>,
    /// Escape sequence buffer
    esc_buf: Vec<u8>,
    /// Whether prediction is enabled
    enabled: bool,
    /// Ignore alternate screen
    ignore_alt_screen: bool,
}

impl InputHandler {
    fn new() -> Self {
        Self {
            utf8_buf: Vec::new(),
            esc_buf: Vec::new(),
            enabled: true,
            ignore_alt_screen: false,
        }
    }

    /// Process input byte and return command if any
    fn handle_byte(&mut self, byte: u8, emulator: &Emulator, adaptive: bool) -> Option<Command> {
        if !self.enabled {
            return None;
        }

        // Check alternate screen
        if emulator.in_alt_screen() && !self.ignore_alt_screen {
            return None;
        }

        // Adaptive mode: only predict when SRTT is high enough or glitch detected
        if adaptive && !emulator.should_predict() {
            trace!("Skipping prediction (adaptive: low SRTT)");
            return None;
        }

        // Handle escape sequences
        if !self.esc_buf.is_empty() {
            return self.handle_escape_byte(byte, emulator);
        }

        // Start escape sequence
        if byte == 0x1b {
            self.esc_buf.push(byte);
            self.utf8_buf.clear();
            return None;
        }

        // Handle backspace (DEL=0x7f or BS=0x08)
        if byte == 0x7f || byte == 0x08 {
            self.utf8_buf.clear();
            return self.handle_backspace(emulator);
        }

        // Handle control characters
        if byte < 0x20 {
            self.utf8_buf.clear();
            return None;
        }

        // Handle UTF-8 multi-byte sequences
        if byte >= 0x80 {
            self.utf8_buf.push(byte);

            if let Ok(s) = std::str::from_utf8(&self.utf8_buf) {
                let chars: Vec<char> = s.chars().collect();
                self.utf8_buf.clear();

                // Return command for first char (usually only one)
                if let Some(&ch) = chars.first() {
                    return Some(self.create_char_command(ch, emulator));
                }
            }

            // Not valid yet, wait for more bytes
            if self.utf8_buf.len() > 4 {
                self.utf8_buf.clear();
            }
            return None;
        }

        // ASCII printable character
        self.utf8_buf.clear();
        let ch = byte as char;
        Some(self.create_char_command(ch, emulator))
    }

    fn handle_escape_byte(&mut self, byte: u8, emulator: &Emulator) -> Option<Command> {
        self.esc_buf.push(byte);

        if self.esc_buf.len() == 2 {
            if byte == b'[' {
                return None; // Continue waiting for CSI sequence
            } else {
                // Not a CSI sequence (e.g., ESC followed by normal char)
                // Re-process this byte as normal input
                self.esc_buf.clear();
                // Process the byte as if it's a normal character
                if (0x20..0x7f).contains(&byte) {
                    return Some(self.create_char_command(byte as char, emulator));
                }
                return None;
            }
        }

        if self.esc_buf.len() >= 3 {
            let result = match byte {
                b'A' | b'B' | b'C' | b'D' => {
                    // Arrow keys - invalidate predictions
                    // Don't create a command, just clear predictions
                    None
                }
                _ => None,
            };
            self.esc_buf.clear();
            return result;
        }

        if self.esc_buf.len() > 8 {
            self.esc_buf.clear();
        }

        None
    }

    fn handle_backspace(&self, _emulator: &Emulator) -> Option<Command> {
        // Backspace is handled specially in the main loop
        // because we need to pop the last Char command first
        None
    }

    fn create_char_command(&self, ch: char, emulator: &Emulator) -> Command {
        let width = ch.width().unwrap_or(1) as u8;
        let (row, col) = emulator.prediction_end_cursor();
        let original = emulator.char_at(row, col);

        Command::char(row, col, ch, original, width)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn set_ignore_alt_screen(&mut self, ignore: bool) {
        self.ignore_alt_screen = ignore;
    }
}

/// Buffer for server output to handle incomplete UTF-8 sequences
struct ServerOutputBuffer {
    /// Buffer for incomplete UTF-8 bytes
    buf: Vec<u8>,
}

impl ServerOutputBuffer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Process incoming data and return bytes safe to output to terminal.
    /// Incomplete UTF-8 sequences at the end are buffered.
    fn process(&mut self, data: &[u8]) -> Vec<u8> {
        // Combine buffered data with new data
        self.buf.extend_from_slice(data);

        // Find the boundary of complete UTF-8 sequences
        let complete_len = self.find_complete_utf8_len();

        if complete_len == 0 {
            return Vec::new();
        }

        // Extract complete bytes
        let complete: Vec<u8> = self.buf.drain(..complete_len).collect();
        complete
    }

    /// Find the length of complete UTF-8 sequences in the buffer.
    /// Returns the number of bytes that form valid UTF-8 sequences.
    fn find_complete_utf8_len(&self) -> usize {
        if self.buf.is_empty() {
            return 0;
        }

        // Check how many trailing bytes might be an incomplete UTF-8 sequence
        let len = self.buf.len();

        // Check the last 1-4 bytes for incomplete sequence
        for i in 1..=4.min(len) {
            let pos = len - i;
            let byte = self.buf[pos];

            // Check if this could be a UTF-8 start byte
            if byte & 0x80 == 0 {
                // ASCII byte - can't be start of incomplete sequence
                // Everything up to and including this is complete
                return len;
            } else if byte & 0xC0 == 0xC0 {
                // This is a UTF-8 start byte (110xxxxx, 1110xxxx, or 11110xxx)
                let expected_len = if byte & 0xF8 == 0xF0 {
                    4 // 11110xxx - 4 bytes
                } else if byte & 0xF0 == 0xE0 {
                    3 // 1110xxxx - 3 bytes
                } else if byte & 0xE0 == 0xC0 {
                    2 // 110xxxxx - 2 bytes
                } else {
                    // Invalid, treat as complete
                    return len;
                };

                let available = len - pos;
                if available < expected_len {
                    // Incomplete UTF-8 sequence - return bytes before it
                    return pos;
                } else {
                    // Sequence is complete
                    return len;
                }
            }
            // Continue checking - this is a continuation byte (10xxxxxx)
        }

        // All trailing bytes are continuation bytes without a start byte
        // Keep them buffered - the start byte might come later
        0
    }
}

/// Main proxy loop: forward data between stdin/stdout and PTY
fn proxy_loop(
    pty: &mut PtyMaster,
    sigwinch_fd: RawFd,
    predict_mode: PredictMode,
) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let stdin_fd = unsafe { BorrowedFd::borrow_raw(stdin.as_raw_fd()) };
    let pty_fd = unsafe { BorrowedFd::borrow_raw(pty.as_raw_fd()) };
    let sigwinch_bfd = unsafe { BorrowedFd::borrow_raw(sigwinch_fd) };

    let mut buf = [0u8; 4096];

    // Get terminal size
    let (cols, rows) = get_terminal_size();
    debug!("Terminal size: {}x{}", cols, rows);

    let mut emulator = Emulator::new(cols, rows);
    let mut input_handler = InputHandler::new();
    let mut server_buf = ServerOutputBuffer::new();

    // Whether to use adaptive prediction (mosh-style SRTT-based)
    let use_adaptive = predict_mode == PredictMode::Auto;

    // Configure prediction mode
    match predict_mode {
        PredictMode::Always => {
            input_handler.set_ignore_alt_screen(true);
            info!("Prediction mode: always (ignoring alternate screen)");
        }
        PredictMode::Auto => {
            info!("Prediction mode: auto (adaptive SRTT-based)");
        }
        PredictMode::Never => {
            input_handler.set_enabled(false);
            info!("Prediction mode: never (passthrough)");
        }
    }

    loop {
        let mut poll_fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),
            PollFd::new(pty_fd, PollFlags::POLLIN),
            PollFd::new(sigwinch_bfd, PollFlags::POLLIN),
        ];

        // Calculate poll timeout based on prediction expiry
        let poll_timeout = match emulator.time_until_expire() {
            Some(duration) => {
                let ms = duration.as_millis().min(60000) as u16;
                PollTimeout::from(ms.max(1))
            }
            None => PollTimeout::NONE,
        };

        // Wait for input
        match poll(&mut poll_fds, poll_timeout) {
            Ok(0) => {
                // Timeout - check for glitch (long-pending predictions)
                if use_adaptive {
                    emulator.check_glitch();
                }
                // Check for expired predictions
                if let Some(restore) = emulator.check_timeout() {
                    trace!("timeout rollback: {:?}", String::from_utf8_lossy(&restore));
                    stdout.write_all(&restore)?;
                    stdout.flush()?;
                }
                continue;
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::other(e)),
        }

        // Check for SIGWINCH (window resize)
        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                // Drain the pipe
                let mut drain_buf = [0u8; 16];
                let _ = unsafe {
                    libc::read(
                        sigwinch_fd,
                        drain_buf.as_mut_ptr() as *mut libc::c_void,
                        drain_buf.len(),
                    )
                };

                // Get new terminal size
                let (new_cols, new_rows) = get_terminal_size();
                debug!("Window resize: {}x{}", new_cols, new_rows);
                let _ = pty.set_winsize(new_cols as u16, new_rows as u16);

                // Rollback predictions and resize emulator
                if emulator.pending_count() > 0 {
                    let restore = emulator.rollback_all();
                    stdout.write_all(&restore)?;
                }
                emulator = Emulator::new(new_cols, new_rows);
            }
        }

        // Check for stdin input
        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let n = unsafe {
                    libc::read(
                        stdin.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }

                let input_data = &buf[..n as usize];
                debug!(
                    "stdin: {} bytes {:?}",
                    n,
                    input_data
                        .iter()
                        .map(|b| {
                            if *b >= 0x20 && *b < 0x7f {
                                format!("'{}'", *b as char)
                            } else {
                                format!("0x{:02x}", b)
                            }
                        })
                        .collect::<Vec<_>>()
                );

                // Process each input byte
                for &byte in input_data {
                    // Check adaptive mode: skip prediction when SRTT is low
                    let should_predict = !use_adaptive || emulator.should_predict();

                    // Handle backspace specially
                    if byte == 0x7f || byte == 0x08 {
                        if should_predict {
                            // Create Backspace prediction
                            // Don't pop Char - both Char and Backspace are sent to server
                            let (row, col) = emulator.prediction_end_cursor();

                            if col > 0 {
                                let target_col = col - 1;
                                let original_char = emulator.char_at(row, target_col);

                                // Create and execute Backspace command
                                let cmd = Command::backspace(row, target_col, original_char, 1);
                                // Determine flagging: always show underline in "always" mode,
                                // or follow adaptive in "auto" mode
                                let flagging = !use_adaptive || emulator.should_flag();
                                let overlay = cmd.execute(flagging);
                                trace!(
                                    "backspace overlay at col {}: {:?}",
                                    target_col,
                                    String::from_utf8_lossy(&overlay)
                                );
                                stdout.write_all(&overlay)?;

                                // Track the command
                                emulator.push_command(cmd);
                            }
                        }
                    } else if let Some(cmd) =
                        input_handler.handle_byte(byte, &emulator, use_adaptive)
                    {
                        // Execute the command (show prediction)
                        // Determine flagging: always show underline in "always" mode,
                        // or follow adaptive in "auto" mode
                        let flagging = !use_adaptive || emulator.should_flag();
                        let overlay = cmd.execute(flagging);
                        trace!("overlay: {:?}", String::from_utf8_lossy(&overlay));
                        stdout.write_all(&overlay)?;

                        // Track the command
                        emulator.push_command(cmd);
                    }
                }
                stdout.flush()?;

                // Forward to PTY
                pty.write_all(input_data)?;
            }
            if revents.contains(PollFlags::POLLHUP) {
                break;
            }
        }

        // Check for PTY output
        if let Some(revents) = poll_fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match pty.read(&mut buf) {
                    Ok(0) => {
                        debug!("PTY closed");
                        break;
                    }
                    Ok(n) => {
                        let raw_data = &buf[..n];
                        debug!("pty: {} bytes {:?}", n, String::from_utf8_lossy(raw_data));

                        // Buffer incomplete UTF-8 sequences
                        let server_data = server_buf.process(raw_data);

                        if server_data.is_empty() {
                            // Only incomplete UTF-8 bytes, wait for more
                            continue;
                        }

                        // Get cursor positions before processing
                        let pending_width = emulator.pending_width();
                        let (server_row, server_col) = emulator.server_cursor();

                        // Feed to emulator and verify FIRST (before passthrough)
                        let result = emulator.feed_and_verify(&server_data);

                        // If rollback needed, do it BEFORE passthrough
                        // This clears predictions so server output won't be overwritten
                        if let VerifyResult::Rollback(undo) = &result {
                            debug!("rollback: {:?}", String::from_utf8_lossy(undo));
                            stdout.write_all(undo)?;
                        }

                        // Move cursor to server position ONLY if we had pending predictions
                        // Otherwise, just passthrough without cursor manipulation
                        if pending_width > 0 {
                            write!(stdout, "\x1b[{};{}H", server_row + 1, server_col + 1)?;
                            trace!("cursor: to server ({}, {})", server_row, server_col);
                        }

                        // Passthrough - server output is now visible
                        stdout.write_all(&server_data)?;

                        // Log confirmations
                        if let VerifyResult::Confirmed(count) = result {
                            debug!("confirmed {} predictions", count);
                        }

                        // Move cursor to prediction end if we have pending predictions
                        let new_pending_width = emulator.pending_width();
                        if new_pending_width > 0 {
                            let (row, col) = emulator.prediction_end_cursor();
                            write!(stdout, "\x1b[{};{}H", row + 1, col + 1)?;
                            trace!("cursor: to prediction end ({}, {})", row, col);
                        }

                        stdout.flush()?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                        break;
                    }
                }
            }
            if revents.contains(PollFlags::POLLHUP) {
                break;
            }
        }
    }

    Ok(())
}

fn print_help() {
    println!(
        r#"{} {} - Local Echo Proxy

Provides mosh-style predictive local echo over SSH without server-side installation.

USAGE:
    lep [OPTIONS] <COMMAND> [ARGS...]

ARGS:
    <COMMAND>    Command to run (e.g., ssh user@host)
    [ARGS...]    Arguments to pass to the command

OPTIONS:
    -h, --help              Print this help message
    -V, --version           Print version information
    -v, --verbose           Increase verbosity (can be repeated: -v, -vv, -vvv)
    --log <FILE>            Write debug logs to FILE
    -p, --predict <MODE>    Prediction mode: always, auto, never [default: auto]
                              always - Always predict (even in vim/less)
                              auto   - Disable prediction in alternate screen
                              never  - Disable prediction entirely (passthrough)

EXAMPLES:
    lep ssh user@host                  # SSH with auto prediction
    lep -p always ssh user@host        # Always predict (even in vim)
    lep -p never ssh user@host         # Pure passthrough, no prediction
    lep --predict=auto ssh user@host   # Explicit auto mode
    lep -v ssh user@host               # With info logging to stderr
    lep --log /tmp/lep.log ssh ...     # Log to file

ENVIRONMENT:
    RUST_LOG    Set log level (error, warn, info, debug, trace)
                Overrides -v flag if set

For more information, visit: https://github.com/anomalyco/lep"#,
        NAME, VERSION
    );
}

fn print_version() {
    println!("{} {}", NAME, VERSION);
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut config = Config {
        command: Vec::new(),
        predict_mode: PredictMode::Auto,
        log_file: None,
        verbose: 0,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                print_version();
                std::process::exit(0);
            }
            "-v" | "--verbose" => {
                config.verbose = config.verbose.saturating_add(1);
            }
            "-vv" => {
                config.verbose = config.verbose.saturating_add(2);
            }
            "-vvv" => {
                config.verbose = config.verbose.saturating_add(3);
            }
            "--log" => {
                i += 1;
                if i >= args.len() {
                    return Err("--log requires a file path".to_string());
                }
                config.log_file = Some(args[i].clone());
            }
            "-p" | "--predict" => {
                i += 1;
                if i >= args.len() {
                    return Err("--predict requires a mode: always, auto, never".to_string());
                }
                config.predict_mode = PredictMode::from_str(&args[i])?;
            }
            _ if arg.starts_with("--predict=") => {
                let mode = arg.strip_prefix("--predict=").unwrap();
                config.predict_mode = PredictMode::from_str(mode)?;
            }
            _ if arg.starts_with("-p=") => {
                let mode = arg.strip_prefix("-p=").unwrap();
                config.predict_mode = PredictMode::from_str(mode)?;
            }
            _ if arg.starts_with('-') => {
                // Check for combined short flags like -vvv
                if arg.starts_with("-v") && arg.chars().skip(1).all(|c| c == 'v') {
                    config.verbose = config.verbose.saturating_add((arg.len() - 1) as u8);
                } else {
                    return Err(format!("Unknown option: {}. Try 'lep --help'", arg));
                }
            }
            _ => {
                // Start of command
                config.command = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }

    if config.command.is_empty() {
        return Err("No command specified".to_string());
    }

    Ok(config)
}

fn setup_logging(config: &Config) {
    use log::LevelFilter;
    use std::sync::Mutex;

    static LOGGER: SimpleLogger = SimpleLogger;
    static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

    struct SimpleLogger;

    impl log::Log for SimpleLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            let msg = format!(
                "[{} {} {}:{}] {}\n",
                chrono_lite(),
                record.level(),
                record.file().unwrap_or("?"),
                record.line().unwrap_or(0),
                record.args()
            );

            if let Ok(mut guard) = LOG_FILE.lock() {
                if let Some(ref mut file) = *guard {
                    let _ = std::io::Write::write_all(file, msg.as_bytes());
                } else {
                    eprint!("{}", msg);
                }
            }
        }

        fn flush(&self) {
            if let Ok(mut guard) = LOG_FILE.lock() {
                if let Some(ref mut file) = *guard {
                    let _ = std::io::Write::flush(file);
                }
            }
        }
    }

    // Set up log file if specified
    if let Some(ref log_file) = config.log_file {
        match OpenOptions::new().create(true).append(true).open(log_file) {
            Ok(file) => {
                if let Ok(mut guard) = LOG_FILE.lock() {
                    *guard = Some(file);
                }
            }
            Err(e) => {
                eprintln!("Warning: Cannot open log file {}: {}", log_file, e);
            }
        }
    }

    let level = match config.verbose {
        0 => LevelFilter::Off,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

/// Simple timestamp without chrono dependency
fn chrono_lite() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    format!("{}.{:03}", secs % 100000, millis)
}

fn main() {
    let config = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            eprintln!("Try 'lep --help' for more information.");
            std::process::exit(1);
        }
    };

    setup_logging(&config);

    info!("Starting {} v{}", NAME, VERSION);
    info!("Command: {:?}", config.command);
    info!("Prediction mode: {:?}", config.predict_mode);

    if let Err(e) = run_proxy(&config) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // ServerOutputBuffer Tests
    // ========================================================================

    #[test]
    fn test_server_buffer_complete_ascii() {
        let mut buf = ServerOutputBuffer::new();

        let result = buf.process(b"hello");
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_server_buffer_complete_utf8() {
        let mut buf = ServerOutputBuffer::new();

        // Complete UTF-8 "안녕"
        let result = buf.process("안녕".as_bytes());
        assert_eq!(result, "안녕".as_bytes());
    }

    #[test]
    fn test_server_buffer_incomplete_utf8() {
        let mut buf = ServerOutputBuffer::new();

        // "가" is 0xEA 0xB0 0x80 in UTF-8
        // Send first 2 bytes (incomplete)
        let result = buf.process(&[0xEA, 0xB0]);
        assert!(result.is_empty()); // Should buffer incomplete bytes

        // Send the last byte
        let result = buf.process(&[0x80]);
        assert_eq!(result, "가".as_bytes());
    }

    #[test]
    fn test_server_buffer_mixed_complete_incomplete() {
        let mut buf = ServerOutputBuffer::new();

        // "ab" + incomplete UTF-8 start
        let result = buf.process(&[b'a', b'b', 0xEA]);
        assert_eq!(result, b"ab"); // Only complete bytes

        // Complete the sequence
        let result = buf.process(&[0xB0, 0x80]);
        assert_eq!(result, "가".as_bytes());
    }

    #[test]
    fn test_server_buffer_escape_sequences() {
        let mut buf = ServerOutputBuffer::new();

        // Escape sequence should pass through
        let result = buf.process(b"\x1b[1;1H");
        assert_eq!(result, b"\x1b[1;1H");
    }

    // ========================================================================
    // PredictMode Tests
    // ========================================================================

    #[test]
    fn test_predict_mode_from_str() {
        assert_eq!(
            PredictMode::from_str("always").unwrap(),
            PredictMode::Always
        );
        assert_eq!(PredictMode::from_str("auto").unwrap(), PredictMode::Auto);
        assert_eq!(PredictMode::from_str("never").unwrap(), PredictMode::Never);
        assert_eq!(PredictMode::from_str("none").unwrap(), PredictMode::Never);
        assert_eq!(PredictMode::from_str("off").unwrap(), PredictMode::Never);
        assert_eq!(
            PredictMode::from_str("ALWAYS").unwrap(),
            PredictMode::Always
        );
        assert!(PredictMode::from_str("invalid").is_err());
    }

    // ========================================================================
    // InputHandler Tests
    // ========================================================================

    #[test]
    fn test_input_handler_ascii() {
        let handler = InputHandler::new();
        let emu = Emulator::new(80, 24);

        // Test that we can create commands for ASCII chars
        let cmd = handler.create_char_command('a', &emu);
        assert!(matches!(
            cmd,
            Command::Char {
                ch: 'a',
                width: 1,
                ..
            }
        ));
    }

    #[test]
    fn test_input_handler_wide_char() {
        let handler = InputHandler::new();
        let emu = Emulator::new(80, 24);

        // Wide character should have width 2
        let cmd = handler.create_char_command('가', &emu);
        assert!(matches!(
            cmd,
            Command::Char {
                ch: '가',
                width: 2,
                ..
            }
        ));
    }

    #[test]
    fn test_input_handler_disabled() {
        let mut handler = InputHandler::new();
        let emu = Emulator::new(80, 24);

        handler.set_enabled(false);

        // When disabled, should return None
        let result = handler.handle_byte(b'a', &emu, false);
        assert!(result.is_none());
    }
}
