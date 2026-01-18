//! Emulator module - manages terminal state and command verification

use crate::command::Command;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::Processor;
use log::debug;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Default prediction timeout (used when no RTT data available)
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum timeout floor
const MIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum timeout ceiling
const MAX_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout multiplier (timeout = SRTT * TIMEOUT_MULTIPLIER)
const TIMEOUT_MULTIPLIER: f64 = 3.0;

/// SRTT smoothing factor (alpha for exponential moving average)
/// New SRTT = alpha * sample + (1 - alpha) * old_srtt
const SRTT_ALPHA: f64 = 0.125;

// ============================================================================
// Adaptive Prediction Constants (from mosh)
// ============================================================================

/// SRTT threshold to enable predictions (> 30ms = enable)
const SRTT_TRIGGER_HIGH: Duration = Duration::from_millis(30);

/// SRTT threshold to disable predictions (<= 20ms = disable)
const SRTT_TRIGGER_LOW: Duration = Duration::from_millis(20);

/// Prediction pending longer than this = glitch (250ms)
const GLITCH_THRESHOLD: Duration = Duration::from_millis(250);

/// Number of quick confirmations needed to heal glitch trigger
const GLITCH_REPAIR_COUNT: u32 = 10;

/// Minimum interval between quick confirmations (150ms)
const GLITCH_REPAIR_MININTERVAL: Duration = Duration::from_millis(150);

/// Prediction pending longer than this = show underline (5s)
const GLITCH_FLAG_THRESHOLD: Duration = Duration::from_secs(5);

/// SRTT threshold to start underlining (> 80ms)
const FLAG_TRIGGER_HIGH: Duration = Duration::from_millis(80);

/// SRTT threshold to stop underlining (<= 50ms)
const FLAG_TRIGGER_LOW: Duration = Duration::from_millis(50);

/// Server events extracted from output data
#[derive(Debug, Clone, PartialEq)]
enum ServerEvent {
    /// Printable character
    Char(char),
}

/// Event listener that does nothing
struct NoOpEventListener;

impl EventListener for NoOpEventListener {
    fn send_event(&self, _event: Event) {}
}

/// Synchronous emulator for terminal state tracking and prediction verification
pub struct Emulator {
    term: Term<NoOpEventListener>,
    processor: Processor,
    commands: VecDeque<(Command, Instant)>,
    alt_screen: bool,
    alt_screen_buf: Vec<u8>,
    /// Smoothed Round Trip Time (for dynamic timeout)
    srtt: Option<Duration>,

    // Adaptive prediction state (mosh-style)
    /// Show predictions because of slow round trip time
    srtt_trigger: bool,
    /// Show predictions because of long-pending prediction (glitch)
    glitch_trigger: u32,
    /// Last time we had a quick confirmation (for healing glitch trigger)
    last_quick_confirmation: Option<Instant>,
    /// Whether to show underline on predictions
    flagging: bool,
}

impl Emulator {
    /// Create a new emulator
    pub fn new(cols: usize, rows: usize) -> Self {
        let size = TermSize::new(cols, rows);
        let term = Term::new(term::Config::default(), &size, NoOpEventListener);

        Self {
            term,
            processor: Processor::new(),
            commands: VecDeque::new(),
            alt_screen: false,
            alt_screen_buf: Vec::new(),
            srtt: None,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: None,
            flagging: false,
        }
    }

    /// Add a command to track
    pub fn push_command(&mut self, cmd: Command) {
        self.commands.push_back((cmd, Instant::now()));
    }

    /// Get pending command count
    pub fn pending_count(&self) -> usize {
        self.commands.len()
    }

    /// Get pending backspace command count
    pub fn pending_backspace_count(&self) -> usize {
        self.commands
            .iter()
            .filter(|(cmd, _)| matches!(cmd, Command::Backspace { .. }))
            .count()
    }

    /// Get total cursor delta from all pending commands
    pub fn pending_cursor_delta(&self) -> i32 {
        self.commands.iter().map(|(c, _)| c.cursor_delta()).sum()
    }

    /// Get total pending width (absolute value for display purposes)
    pub fn pending_width(&self) -> usize {
        self.pending_cursor_delta().unsigned_abs() as usize
    }

    /// Get server cursor position
    pub fn server_cursor(&self) -> (usize, usize) {
        let cursor = self.term.grid().cursor.point;
        (cursor.line.0 as usize, cursor.column.0)
    }

    /// Get prediction end cursor position
    pub fn prediction_end_cursor(&self) -> (usize, usize) {
        let (row, col) = self.server_cursor();
        let delta = self.pending_cursor_delta();
        if delta >= 0 {
            (row, col + delta as usize)
        } else {
            (row, col.saturating_sub((-delta) as usize))
        }
    }

    /// Feed server output and verify predictions against server message
    ///
    /// Compare server output directly with predictions:
    /// - Char prediction: expects matching printable char at correct position
    /// - Backspace prediction: expects backspace effect (cursor left, or \x08/\x7f)
    /// - Mismatch = rollback all predictions
    pub fn feed_and_verify(&mut self, data: &[u8]) -> VerifyResult {
        // Detect alternate screen
        for &byte in data {
            self.detect_alt_screen(byte);
        }

        // Save cursor position BEFORE feeding
        let cursor_before = self.server_cursor();

        // Feed to emulator
        self.processor.advance(&mut self.term, data);

        // Get cursor position AFTER feeding
        let cursor_after = self.server_cursor();

        // No pending predictions? Nothing to verify
        if self.commands.is_empty() {
            return VerifyResult::Pending;
        }

        // Extract meaningful events from server data
        let events = self.extract_server_events(data);

        // If no meaningful events (no printable chars), check cursor movement
        if events.is_empty() {
            // Check if cursor moved
            let cursor_moved = cursor_after != cursor_before;

            if cursor_moved {
                // First check: if we have Char predictions pending, cursor-only movement = rollback
                // This handles vim hjkl where 'h' moves cursor instead of inserting char
                if let Some((cmd, created_at)) = self.commands.front() {
                    if matches!(cmd, Command::Char { .. }) {
                        // Server moved cursor without printing char - prediction failed
                        // Still measure RTT from rollback (server did respond)
                        let rtt = created_at.elapsed();
                        let undo = self.rollback_all();
                        self.update_srtt(rtt);
                        return VerifyResult::Rollback(undo);
                    }
                }

                // Cursor moved left - could confirm backspace predictions
                if cursor_after.0 == cursor_before.0 && cursor_after.1 < cursor_before.1 {
                    let cols_moved = cursor_before.1 - cursor_after.1;
                    return self.verify_backspaces(cols_moved);
                }
            }
            return VerifyResult::Pending;
        }

        // Verify each event against predictions
        let mut confirmed = 0;
        let mut current_col = cursor_before.1;
        let current_row = cursor_before.0;

        for event in events {
            if let Some((cmd, _)) = self.commands.front() {
                match (&event, cmd) {
                    // Printable char vs Char prediction
                    (
                        ServerEvent::Char(ch),
                        Command::Char {
                            row,
                            col,
                            ch: predicted_ch,
                            width,
                            ..
                        },
                    ) => {
                        if *row == current_row && *col == current_col && predicted_ch == ch {
                            // Match! Confirmed - measure RTT
                            let char_width = *width as usize; // Copy before mutable borrow
                            if let Some((_, created_at)) = self.commands.pop_front() {
                                self.update_srtt(created_at.elapsed());
                            }
                            confirmed += 1;
                            current_col += char_width; // Wide chars move cursor by width
                        } else {
                            // Mismatch - rollback, but still measure RTT
                            let rtt = self.commands.front().map(|(_, t)| t.elapsed());
                            let undo = self.rollback_all();
                            if let Some(rtt) = rtt {
                                self.update_srtt(rtt);
                            }
                            return VerifyResult::Rollback(undo);
                        }
                    }
                    // Printable char vs Backspace prediction - contradiction
                    (ServerEvent::Char(_), Command::Backspace { .. }) => {
                        // Measure RTT even on contradiction
                        let rtt = self.commands.front().map(|(_, t)| t.elapsed());
                        let undo = self.rollback_all();
                        if let Some(rtt) = rtt {
                            self.update_srtt(rtt);
                        }
                        return VerifyResult::Rollback(undo);
                    }
                    // CursorMove handling
                    (_, Command::CursorMove { .. }) => {
                        break;
                    }
                }
            } else {
                // No more predictions
                break;
            }
        }

        if confirmed > 0 {
            VerifyResult::Confirmed(confirmed)
        } else {
            VerifyResult::Pending
        }
    }

    /// Verify backspace predictions based on cursor movement
    fn verify_backspaces(&mut self, cols_moved: usize) -> VerifyResult {
        let mut confirmed = 0;
        for _ in 0..cols_moved {
            if let Some((cmd, _)) = self.commands.front() {
                if matches!(cmd, Command::Backspace { .. }) {
                    // Measure RTT
                    if let Some((_, created_at)) = self.commands.pop_front() {
                        self.update_srtt(created_at.elapsed());
                    }
                    confirmed += 1;
                } else {
                    // Expected backspace but got something else
                    break;
                }
            } else {
                break;
            }
        }
        if confirmed > 0 {
            VerifyResult::Confirmed(confirmed)
        } else {
            VerifyResult::Pending
        }
    }

    /// Extract meaningful events from server data
    fn extract_server_events(&self, data: &[u8]) -> Vec<ServerEvent> {
        let mut result = Vec::new();
        let mut i = 0;

        while i < data.len() {
            let byte = data[i];

            // Skip escape sequences
            if byte == 0x1b {
                i += 1;
                // Skip until end of escape sequence
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    // CSI sequence ends with letter
                    if b.is_ascii_alphabetic() || b == b'~' {
                        break;
                    }
                }
                continue;
            }

            // Skip control characters (including 0x08, 0x7f)
            // Backspace "effect" is detected by cursor movement, not by character
            if byte < 0x20 || byte == 0x7f {
                i += 1;
                continue;
            }

            // Try to decode UTF-8 character
            let remaining = &data[i..];
            if let Some((ch, len)) = decode_utf8_char(remaining) {
                result.push(ServerEvent::Char(ch));
                i += len;
            } else {
                i += 1;
            }
        }

        result
    }

    /// Rollback all pending commands
    pub fn rollback_all(&mut self) -> Vec<u8> {
        let mut output = Vec::new();

        while let Some((cmd, _)) = self.commands.pop_back() {
            output.extend(cmd.undo(&self.term));
        }

        // Move cursor to server position
        let cursor = self.term.grid().cursor.point;
        output.extend_from_slice(
            format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
        );

        output
    }

    /// Pop the last command (for backspace)
    pub fn pop_command(&mut self) -> Option<Command> {
        self.commands.pop_back().map(|(cmd, _)| cmd)
    }

    /// Find and remove the last Char command, returning the command
    /// Used for backspace to undo the last character
    pub fn pop_last_char_command(&mut self) -> Option<Command> {
        // Find the last Char command from the back
        let mut idx = None;
        for (i, (cmd, _)) in self.commands.iter().enumerate().rev() {
            if cmd.is_char() {
                idx = Some(i);
                break;
            }
        }

        if let Some(i) = idx {
            let (cmd, _) = self.commands.remove(i)?;
            Some(cmd)
        } else {
            None
        }
    }

    /// Check for timeout
    pub fn check_timeout(&mut self) -> Option<Vec<u8>> {
        let timeout = self.prediction_timeout();
        if let Some((_, created_at)) = self.commands.front() {
            if created_at.elapsed() >= timeout {
                return Some(self.rollback_all());
            }
        }
        None
    }

    /// Get time until oldest command expires
    pub fn time_until_expire(&self) -> Option<Duration> {
        let timeout = self.prediction_timeout();
        self.commands.front().map(|(_, created_at)| {
            let elapsed = created_at.elapsed();
            if elapsed >= timeout {
                Duration::ZERO
            } else {
                timeout - elapsed
            }
        })
    }

    /// Get dynamic prediction timeout based on SRTT
    fn prediction_timeout(&self) -> Duration {
        match self.srtt {
            Some(srtt) => srtt
                .mul_f64(TIMEOUT_MULTIPLIER)
                .clamp(MIN_TIMEOUT, MAX_TIMEOUT),
            None => DEFAULT_TIMEOUT,
        }
    }

    /// Update SRTT with a new RTT sample and adjust triggers
    fn update_srtt(&mut self, rtt_sample: Duration) {
        let now = Instant::now();

        match self.srtt {
            Some(old_srtt) => {
                // Exponential moving average
                let old_nanos = old_srtt.as_nanos() as f64;
                let sample_nanos = rtt_sample.as_nanos() as f64;
                let new_nanos = SRTT_ALPHA * sample_nanos + (1.0 - SRTT_ALPHA) * old_nanos;
                self.srtt = Some(Duration::from_nanos(new_nanos as u64));
            }
            None => {
                self.srtt = Some(rtt_sample);
            }
        }

        // Update adaptive triggers based on new SRTT
        self.update_triggers();

        // Quick confirmation heals glitch trigger (mosh logic)
        if rtt_sample < GLITCH_THRESHOLD && self.glitch_trigger > 0 {
            let should_heal = match self.last_quick_confirmation {
                Some(last) => now.duration_since(last) >= GLITCH_REPAIR_MININTERVAL,
                None => true,
            };
            if should_heal {
                self.glitch_trigger = self.glitch_trigger.saturating_sub(1);
                self.last_quick_confirmation = Some(now);
                debug!(
                    "Glitch trigger healed: {} (rtt={:?})",
                    self.glitch_trigger, rtt_sample
                );
            }
        }
    }

    /// Update srtt_trigger and flagging based on current SRTT (with hysteresis)
    fn update_triggers(&mut self) {
        if let Some(srtt) = self.srtt {
            // SRTT trigger with hysteresis
            if srtt > SRTT_TRIGGER_HIGH {
                self.srtt_trigger = true;
            } else if srtt <= SRTT_TRIGGER_LOW && !self.has_pending() {
                // Only turn off when no predictions being shown
                self.srtt_trigger = false;
            }

            // Flagging (underline) with hysteresis
            if srtt > FLAG_TRIGGER_HIGH {
                self.flagging = true;
            } else if srtt <= FLAG_TRIGGER_LOW {
                self.flagging = false;
            }

            // Really big glitch also activates underlining
            if self.glitch_trigger > GLITCH_REPAIR_COUNT {
                self.flagging = true;
            }
        }
    }

    /// Check for long-pending predictions and update glitch trigger
    pub fn check_glitch(&mut self) {
        if let Some((_, created_at)) = self.commands.front() {
            let pending_time = created_at.elapsed();

            if pending_time >= GLITCH_FLAG_THRESHOLD {
                // Very long pending = force flagging
                self.glitch_trigger = GLITCH_REPAIR_COUNT * 2;
                self.flagging = true;
            } else if pending_time >= GLITCH_THRESHOLD && self.glitch_trigger < GLITCH_REPAIR_COUNT
            {
                // Long pending = activate glitch trigger
                self.glitch_trigger = GLITCH_REPAIR_COUNT;
                debug!("Glitch detected: pending={:?}", pending_time);
            }
        }
    }

    /// Should we show predictions? (Adaptive mode)
    /// Returns true if SRTT is high enough or glitch trigger is active
    pub fn should_predict(&self) -> bool {
        self.srtt_trigger || self.glitch_trigger > 0
    }

    /// Should we show underline on predictions?
    pub fn should_flag(&self) -> bool {
        self.flagging
    }

    /// Get current SRTT (for debugging/display)
    pub fn get_srtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// Check if there are pending predictions
    fn has_pending(&self) -> bool {
        !self.commands.is_empty()
    }

    /// Reset adaptive state (called on repeated failures)
    pub fn reset_adaptive(&mut self) {
        self.srtt_trigger = false;
        self.glitch_trigger = 0;
        self.flagging = false;
        debug!("Adaptive state reset");
    }

    /// Check if in alternate screen
    pub fn in_alt_screen(&self) -> bool {
        self.alt_screen
    }

    /// Get character at position
    pub fn char_at(&self, row: usize, col: usize) -> char {
        self.term.grid()[Line(row as i32)][Column(col)].c
    }

    fn detect_alt_screen(&mut self, byte: u8) {
        match self.alt_screen_buf.len() {
            0 => {
                if byte == 0x1b {
                    self.alt_screen_buf.push(byte);
                }
            }
            1 => {
                if byte == b'[' {
                    self.alt_screen_buf.push(byte);
                } else {
                    self.alt_screen_buf.clear();
                }
            }
            2 => {
                if byte == b'?' {
                    self.alt_screen_buf.push(byte);
                } else {
                    self.alt_screen_buf.clear();
                }
            }
            _ => {
                self.alt_screen_buf.push(byte);

                if byte == b'h' || byte == b'l' {
                    let seq = &self.alt_screen_buf[3..self.alt_screen_buf.len() - 1];
                    if seq == b"1049" || seq == b"47" {
                        self.alt_screen = byte == b'h';
                        if self.alt_screen {
                            self.commands.clear();
                        }
                    }
                    self.alt_screen_buf.clear();
                } else if !byte.is_ascii_digit() || self.alt_screen_buf.len() > 10 {
                    self.alt_screen_buf.clear();
                }
            }
        }
    }
}

/// Result of verification
#[derive(Debug)]
pub enum VerifyResult {
    /// Commands confirmed
    Confirmed(usize),
    /// Still pending
    Pending,
    /// Need rollback
    Rollback(Vec<u8>),
}

/// Decode a single UTF-8 character from a byte slice
/// Returns the character and the number of bytes consumed
fn decode_utf8_char(data: &[u8]) -> Option<(char, usize)> {
    if data.is_empty() {
        return None;
    }

    let first = data[0];

    // ASCII (single byte)
    if first & 0x80 == 0 {
        return Some((first as char, 1));
    }

    // Multi-byte UTF-8
    let (len, mask) = if first & 0xE0 == 0xC0 {
        (2, 0x1F) // 110xxxxx
    } else if first & 0xF0 == 0xE0 {
        (3, 0x0F) // 1110xxxx
    } else if first & 0xF8 == 0xF0 {
        (4, 0x07) // 11110xxx
    } else {
        return None;
    };

    if data.len() < len {
        return None;
    }

    // Build code point from first byte
    let mut code_point = (first & mask) as u32;

    // Add continuation bytes
    for &byte in data.iter().take(len).skip(1) {
        if byte & 0xC0 != 0x80 {
            return None;
        }
        code_point = (code_point << 6) | (byte & 0x3F) as u32;
    }

    char::from_u32(code_point).map(|ch| (ch, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_emulator_char_confirmed() {
        let mut emu = Emulator::new(80, 24);

        // Add prediction
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        emu.push_command(cmd);
        assert_eq!(emu.pending_count(), 1);

        // Server echoes 'a'
        let result = emu.feed_and_verify(b"a");
        assert!(matches!(result, VerifyResult::Confirmed(1)));
        assert_eq!(emu.pending_count(), 0);
    }

    #[test]
    fn test_emulator_char_failed() {
        let mut emu = Emulator::new(80, 24);

        // Add prediction
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        emu.push_command(cmd);

        // Server echoes 'x' instead
        let result = emu.feed_and_verify(b"x");
        assert!(matches!(result, VerifyResult::Rollback(_)));
        assert_eq!(emu.pending_count(), 0);
    }

    #[test]
    fn test_emulator_multiple_commands() {
        let mut emu = Emulator::new(80, 24);

        // Add predictions
        emu.push_command(Command::char(0, 0, 'a', ' ', 1));
        emu.push_command(Command::char(0, 1, 'b', ' ', 1));
        emu.push_command(Command::char(0, 2, 'c', ' ', 1));
        assert_eq!(emu.pending_count(), 3);

        // Server echoes 'a'
        let result = emu.feed_and_verify(b"a");
        assert!(matches!(result, VerifyResult::Confirmed(1)));
        assert_eq!(emu.pending_count(), 2);

        // Server echoes 'bc'
        let result = emu.feed_and_verify(b"bc");
        assert!(matches!(result, VerifyResult::Confirmed(2)));
        assert_eq!(emu.pending_count(), 0);
    }

    #[test]
    fn test_emulator_server_cursor() {
        let mut emu = Emulator::new(80, 24);

        assert_eq!(emu.server_cursor(), (0, 0));

        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));
    }

    #[test]
    fn test_emulator_pending_width() {
        let mut emu = Emulator::new(80, 24);

        emu.push_command(Command::char(0, 0, 'a', ' ', 1));
        emu.push_command(Command::char(0, 1, '가', ' ', 2));
        assert_eq!(emu.pending_width(), 3);
    }

    #[test]
    fn test_emulator_alt_screen() {
        let mut emu = Emulator::new(80, 24);

        emu.push_command(Command::char(0, 0, 'a', ' ', 1));
        assert_eq!(emu.pending_count(), 1);

        // Enter alternate screen
        emu.feed_and_verify(b"\x1b[?1049h");
        assert!(emu.in_alt_screen());
        assert_eq!(emu.pending_count(), 0); // cleared

        // Exit alternate screen
        emu.feed_and_verify(b"\x1b[?1049l");
        assert!(!emu.in_alt_screen());
    }

    /// Backspace uses absolute positioning - prediction_end_cursor is NOT affected
    ///
    /// With the new design:
    /// - Backspace has cursor_delta() = 0
    /// - prediction_end_cursor() stays at server_cursor position
    /// - Backspace stores absolute target position (col where space will appear)
    #[test]
    fn test_backspace_confirmed_clears_delta() {
        let mut emu = Emulator::new(80, 24);

        // Server echoes "hello " (simulating already confirmed chars)
        emu.feed_and_verify(b"hello ");
        assert_eq!(emu.server_cursor(), (0, 6));
        assert_eq!(emu.prediction_end_cursor(), (0, 6));

        // User presses backspace - Backspace command pushed
        // col=5 is the target position (where space will appear)
        let bs_cmd = Command::backspace(0, 5, ' ', 1);
        emu.push_command(bs_cmd);

        // prediction_end_cursor moves left by 1 (delta = -1)
        assert_eq!(emu.prediction_end_cursor(), (0, 5));

        // Server processes backspace (sends "\b \b")
        emu.feed_and_verify(b"\x08 \x08"); // backspace, space, backspace

        // After server confirms backspace, pending should be empty
        assert_eq!(emu.pending_count(), 0);
        assert_eq!(emu.server_cursor(), (0, 5));
        assert_eq!(emu.prediction_end_cursor(), (0, 5));
    }

    /// Multiple backspaces accumulate delta
    ///
    /// Each Backspace has delta=-1, so prediction_end_cursor moves left
    #[test]
    fn test_multiple_backspace_targets_correct_positions() {
        let mut emu = Emulator::new(80, 24);

        // Server echoes "hello"
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));
        assert_eq!(emu.char_at(0, 4), 'o');
        assert_eq!(emu.char_at(0, 3), 'l');
        assert_eq!(emu.char_at(0, 2), 'l');

        // First backspace: targets col 4 (where 'o' is), original='o'
        let bs1 = Command::backspace(0, 4, 'o', 1);
        emu.push_command(bs1);
        // prediction_end_cursor moves left by 1
        assert_eq!(emu.prediction_end_cursor(), (0, 4));

        // Second backspace: targets col 3 (where 'l' is), original='l'
        let bs2 = Command::backspace(0, 3, 'l', 1);
        emu.push_command(bs2);
        assert_eq!(emu.prediction_end_cursor(), (0, 3));

        // Third backspace: targets col 2 (where 'l' is), original='l'
        let bs3 = Command::backspace(0, 2, 'l', 1);
        emu.push_command(bs3);
        assert_eq!(emu.prediction_end_cursor(), (0, 2));

        // All backspaces should be Pending
        assert_eq!(emu.pending_count(), 3);
    }

    /// With absolute positioning, main.rs must track backspace target position separately
    ///
    /// Since prediction_end_cursor() doesn't change with Backspace,
    /// main.rs needs to track how many pending backspaces exist to compute target
    /// Backspace uses prediction_end_cursor for target calculation
    ///
    /// Now that Backspace has delta=-1, prediction_end_cursor updates automatically
    #[test]
    fn test_backspace_char_at_uses_correct_position() {
        let mut emu = Emulator::new(80, 24);

        // Server echoes "hello"
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));

        // First backspace: prediction_end is (0, 5), target = 5 - 1 = 4
        let (row, col) = emu.prediction_end_cursor();
        assert_eq!((row, col), (0, 5));
        let target_col = col - 1; // 4
        let original_char = emu.char_at(row, target_col);
        assert_eq!(original_char, 'o', "First backspace should target 'o'");

        let bs1 = Command::backspace(row, target_col, original_char, 1);
        emu.push_command(bs1);

        // prediction_end_cursor moves left (delta=-1)
        assert_eq!(emu.prediction_end_cursor(), (0, 4));
        assert_eq!(emu.server_cursor(), (0, 5)); // server unchanged

        // Second backspace: prediction_end is now (0, 4), target = 4 - 1 = 3
        let (row, col) = emu.prediction_end_cursor();
        let target_col = col - 1; // 3
        let original_char = emu.char_at(row, target_col);
        assert_eq!(original_char, 'l', "Second backspace should target 'l'");

        let bs2 = Command::backspace(row, target_col, original_char, 1);
        emu.push_command(bs2);

        assert_eq!(emu.prediction_end_cursor(), (0, 3));
        assert_eq!(emu.pending_count(), 2);
    }

    /// What happens when server echoes \x7f (DEL)?
    #[test]
    fn test_server_echoes_del() {
        let mut emu = Emulator::new(80, 24);

        // Server echoes "hello"
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));
        assert_eq!(emu.char_at(0, 4), 'o');

        // Server echoes \x7f (DEL)
        emu.feed_and_verify(b"\x7f");

        // What does the emulator show now?
        eprintln!("cursor after DEL: {:?}", emu.server_cursor());
        eprintln!("char at 0,4: {:?}", emu.char_at(0, 4));
        eprintln!("char at 0,5: {:?}", emu.char_at(0, 5));
    }

    /// Multiple backspaces accumulate delta
    ///
    /// prediction_end_cursor moves left with each backspace
    #[test]
    fn test_multiple_backspace_verify_bug() {
        let mut emu = Emulator::new(80, 24);

        // Server echoes "hello " (6 chars, cursor at col 6)
        emu.feed_and_verify(b"hello ");
        assert_eq!(emu.server_cursor(), (0, 6));

        // User types 3 backspaces
        // Backspace 1: targets col 5 (where ' ' is), original=' '
        let bs1 = Command::backspace(0, 5, ' ', 1);
        emu.push_command(bs1);

        // Backspace 2: targets col 4 (where 'o' is), original='o'
        let bs2 = Command::backspace(0, 4, 'o', 1);
        emu.push_command(bs2);

        // Backspace 3: targets col 3 (where 'l' is), original='l'
        let bs3 = Command::backspace(0, 3, 'l', 1);
        emu.push_command(bs3);

        assert_eq!(emu.pending_count(), 3);
        // prediction_end_cursor moves left by 3 (delta=-1 each)
        assert_eq!(emu.prediction_end_cursor(), (0, 3));

        // Server echoes \x7f - this doesn't change emulator state much
        let result = emu.feed_and_verify(b"\x7f");
        eprintln!(
            "After first \\x7f: result={:?}, pending={}",
            result,
            emu.pending_count()
        );
    }

    /// vim hjkl: Char prediction + cursor move (no printable char) = rollback
    ///
    /// In vim normal mode, pressing 'h' moves cursor left instead of inserting 'h'.
    /// If we predicted 'h' as a Char, server response will be cursor move only.
    /// This should trigger rollback.
    #[test]
    fn test_vim_hjkl_rollback() {
        let mut emu = Emulator::new(80, 24);

        // Set up: cursor at col 5 (simulate being in middle of line)
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));

        // User types 'h' - we predict it as Char insertion
        let cmd = Command::char(0, 5, 'h', ' ', 1);
        emu.push_command(cmd);
        assert_eq!(emu.pending_count(), 1);

        // vim sends cursor left escape sequence instead of 'h'
        // \x1b[D = cursor left
        let result = emu.feed_and_verify(b"\x1b[D");

        // Should rollback because Char prediction got cursor move instead
        assert!(matches!(result, VerifyResult::Rollback(_)));
        assert_eq!(emu.pending_count(), 0);
        // Cursor should be at col 4 now (moved left by vim)
        assert_eq!(emu.server_cursor(), (0, 4));
    }

    /// vim 'l' (cursor right): same pattern as 'h'
    #[test]
    fn test_vim_l_cursor_right_rollback() {
        let mut emu = Emulator::new(80, 24);

        // Set up: cursor at col 3
        emu.feed_and_verify(b"abc");
        assert_eq!(emu.server_cursor(), (0, 3));

        // Move cursor back to col 1 (simulate being in middle)
        emu.feed_and_verify(b"\x1b[1;2H"); // move to col 1 (0-indexed)
        assert_eq!(emu.server_cursor(), (0, 1));

        // User types 'l' - we predict it as Char insertion
        let cmd = Command::char(0, 1, 'l', 'b', 1);
        emu.push_command(cmd);

        // vim sends cursor right escape sequence
        // \x1b[C = cursor right
        let result = emu.feed_and_verify(b"\x1b[C");

        // Should rollback
        assert!(matches!(result, VerifyResult::Rollback(_)));
        assert_eq!(emu.pending_count(), 0);
    }

    /// vim 'j' (cursor down): Char prediction + row change = rollback
    #[test]
    fn test_vim_j_cursor_down_rollback() {
        let mut emu = Emulator::new(80, 24);

        // Set up: two lines, cursor at row 0
        emu.feed_and_verify(b"line1\r\nline2");
        emu.feed_and_verify(b"\x1b[1;3H"); // move to row 0, col 2
        assert_eq!(emu.server_cursor(), (0, 2));

        // User types 'j' - we predict it as Char insertion
        let cmd = Command::char(0, 2, 'j', 'n', 1);
        emu.push_command(cmd);

        // vim sends cursor down
        // \x1b[B = cursor down
        let result = emu.feed_and_verify(b"\x1b[B");

        // Should rollback
        assert!(matches!(result, VerifyResult::Rollback(_)));
        assert_eq!(emu.pending_count(), 0);
    }

    // ========================================================================
    // Adaptive Prediction Tests
    // ========================================================================

    #[test]
    fn test_adaptive_initial_state() {
        let emu = Emulator::new(80, 24);

        // Initially, no SRTT data, so should_predict is false
        assert!(!emu.should_predict());
        assert!(!emu.should_flag());
        assert!(emu.get_srtt().is_none());
    }

    #[test]
    fn test_adaptive_srtt_trigger() {
        let mut emu = Emulator::new(80, 24);

        // Add a prediction
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        emu.push_command(cmd);

        // Simulate slow RTT (50ms) - should trigger prediction
        std::thread::sleep(Duration::from_millis(50));
        let result = emu.feed_and_verify(b"a");
        assert!(matches!(result, VerifyResult::Confirmed(1)));

        // SRTT should be set now
        assert!(emu.get_srtt().is_some());
        let srtt = emu.get_srtt().unwrap();

        // If SRTT > 30ms, should_predict should be true
        if srtt > SRTT_TRIGGER_HIGH {
            assert!(emu.should_predict());
        }
    }

    #[test]
    fn test_adaptive_glitch_trigger() {
        let mut emu = Emulator::new(80, 24);

        // Initially no glitch
        assert_eq!(emu.glitch_trigger, 0);

        // Add a long-pending prediction (simulate glitch)
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        emu.push_command(cmd);

        // Manually check glitch after threshold
        // Note: can't actually wait 250ms in unit test, so we test the method exists
        emu.check_glitch();

        // In real scenario, after 250ms pending, glitch_trigger would be set
    }

    #[test]
    fn test_adaptive_reset() {
        let mut emu = Emulator::new(80, 24);

        // Manually set some state
        emu.srtt_trigger = true;
        emu.glitch_trigger = 5;
        emu.flagging = true;

        // Reset
        emu.reset_adaptive();

        // All adaptive state should be cleared
        assert!(!emu.srtt_trigger);
        assert_eq!(emu.glitch_trigger, 0);
        assert!(!emu.flagging);
    }

    #[test]
    fn test_adaptive_flagging_hysteresis() {
        let mut emu = Emulator::new(80, 24);

        // Set high SRTT to trigger flagging
        emu.srtt = Some(Duration::from_millis(100)); // > FLAG_TRIGGER_HIGH (80ms)
        emu.update_triggers();
        assert!(emu.should_flag());

        // Lower SRTT below FLAG_TRIGGER_LOW
        emu.srtt = Some(Duration::from_millis(40)); // < FLAG_TRIGGER_LOW (50ms)
        emu.update_triggers();
        assert!(!emu.should_flag());
    }

    #[test]
    fn test_rtt_measured_on_rollback() {
        let mut emu = Emulator::new(80, 24);

        // No SRTT initially
        assert!(emu.get_srtt().is_none());

        // Set up: cursor at col 5
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));

        // User types 'h' - we predict it as Char insertion
        let cmd = Command::char(0, 5, 'h', ' ', 1);
        emu.push_command(cmd);

        // Small delay to get measurable RTT
        std::thread::sleep(Duration::from_millis(10));

        // vim sends cursor left - triggers rollback
        let result = emu.feed_and_verify(b"\x1b[D");
        assert!(matches!(result, VerifyResult::Rollback(_)));

        // SRTT should now be set even though it was a rollback
        assert!(emu.get_srtt().is_some());
        let srtt = emu.get_srtt().unwrap();
        assert!(srtt >= Duration::from_millis(10));
    }

    #[test]
    fn test_rtt_measured_on_char_mismatch() {
        let mut emu = Emulator::new(80, 24);

        // No SRTT initially
        assert!(emu.get_srtt().is_none());

        // Add prediction for 'a'
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        emu.push_command(cmd);

        // Small delay
        std::thread::sleep(Duration::from_millis(10));

        // Server echoes 'x' instead - mismatch rollback
        let result = emu.feed_and_verify(b"x");
        assert!(matches!(result, VerifyResult::Rollback(_)));

        // SRTT should be set
        assert!(emu.get_srtt().is_some());
    }

    // ========================================================================
    // Wide Character Tests
    // ========================================================================

    #[test]
    fn test_wide_char_confirmed() {
        let mut emu = Emulator::new(80, 24);

        // Add prediction for wide character '가' (width 2)
        let cmd = Command::char(0, 0, '가', ' ', 2);
        emu.push_command(cmd);
        assert_eq!(emu.pending_count(), 1);

        // Server echoes '가'
        let result = emu.feed_and_verify("가".as_bytes());
        assert!(matches!(result, VerifyResult::Confirmed(1)));
        assert_eq!(emu.pending_count(), 0);
        // Cursor should be at col 2 (moved by width)
        assert_eq!(emu.server_cursor(), (0, 2));
    }

    #[test]
    fn test_multiple_wide_chars_confirmed() {
        let mut emu = Emulator::new(80, 24);

        // Add predictions for "안녕" (2 wide chars, each width 2)
        emu.push_command(Command::char(0, 0, '안', ' ', 2));
        emu.push_command(Command::char(0, 2, '녕', ' ', 2));
        assert_eq!(emu.pending_count(), 2);

        // Server echoes both
        let result = emu.feed_and_verify("안녕".as_bytes());
        assert!(matches!(result, VerifyResult::Confirmed(2)));
        assert_eq!(emu.pending_count(), 0);
        assert_eq!(emu.server_cursor(), (0, 4));
    }

    #[test]
    fn test_mixed_ascii_wide_chars() {
        let mut emu = Emulator::new(80, 24);

        // "a가b" - ascii, wide, ascii
        emu.push_command(Command::char(0, 0, 'a', ' ', 1));
        emu.push_command(Command::char(0, 1, '가', ' ', 2));
        emu.push_command(Command::char(0, 3, 'b', ' ', 1));
        assert_eq!(emu.pending_count(), 3);

        // Server echoes "a가b"
        let result = emu.feed_and_verify("a가b".as_bytes());
        assert!(matches!(result, VerifyResult::Confirmed(3)));
        assert_eq!(emu.server_cursor(), (0, 4));
    }

    // ========================================================================
    // Timeout Tests
    // ========================================================================

    #[test]
    fn test_timeout_no_pending() {
        let mut emu = Emulator::new(80, 24);

        // No pending commands, timeout should return None
        let result = emu.check_timeout();
        assert!(result.is_none());
    }

    #[test]
    fn test_time_until_expire() {
        let mut emu = Emulator::new(80, 24);

        // No pending, should be None
        assert!(emu.time_until_expire().is_none());

        // Add command
        emu.push_command(Command::char(0, 0, 'a', ' ', 1));

        // Should have some time until expire
        let time = emu.time_until_expire();
        assert!(time.is_some());
        assert!(time.unwrap() > Duration::ZERO);
    }

    // ========================================================================
    // Rollback Tests
    // ========================================================================

    #[test]
    fn test_rollback_restores_original() {
        let mut emu = Emulator::new(80, 24);

        // Server has "hello"
        emu.feed_and_verify(b"hello");
        assert_eq!(emu.server_cursor(), (0, 5));

        // Add prediction at col 5
        emu.push_command(Command::char(0, 5, 'x', ' ', 1));

        // Rollback
        let undo = emu.rollback_all();

        // Should contain cursor positioning
        let undo_str = String::from_utf8_lossy(&undo);
        assert!(undo_str.contains("\x1b[")); // escape sequence
        assert_eq!(emu.pending_count(), 0);
    }
}
