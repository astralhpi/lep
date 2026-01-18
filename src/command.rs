//! Command pattern for predictions
//!
//! Each user input (char, backspace, cursor move, etc.) is modeled as a Command
//! that can be executed (show prediction) and undone (rollback).

use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;

/// Cursor movement direction
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A prediction command that can be executed and undone
#[derive(Debug, Clone)]
pub enum Command {
    /// Character input prediction
    Char {
        row: usize,
        col: usize,
        ch: char,
        original: char,
        width: u8,
    },
    /// Backspace prediction - overlays space at target position
    /// Uses absolute position like Char (no delta accumulation)
    Backspace {
        row: usize,
        /// Target column (where space overlay is shown)
        col: usize,
        /// Original character at this position (for undo)
        original: char,
        /// Width of original character
        width: u8,
    },
    /// Cursor movement prediction
    CursorMove {
        /// Starting position
        from_row: usize,
        from_col: usize,
        /// Direction of movement
        direction: Direction,
    },
}

impl Command {
    /// Create a new Char command
    pub fn char(row: usize, col: usize, ch: char, original: char, width: u8) -> Self {
        Command::Char {
            row,
            col,
            ch,
            original,
            width,
        }
    }

    /// Create a new Backspace command
    /// Create a new Backspace command (absolute position, like Char)
    pub fn backspace(row: usize, col: usize, original: char, width: u8) -> Self {
        Command::Backspace {
            row,
            col,
            original,
            width,
        }
    }

    /// Create a new CursorMove command
    pub fn cursor_move(from_row: usize, from_col: usize, direction: Direction) -> Self {
        Command::CursorMove {
            from_row,
            from_col,
            direction,
        }
    }

    /// Execute the command - returns bytes to output for prediction overlay
    /// If `flagging` is true, underline is added to indicate uncertain prediction
    pub fn execute(&self, flagging: bool) -> Vec<u8> {
        match self {
            Command::Char { row, col, ch, .. } => {
                let mut output = Vec::new();
                // Move to position (1-based)
                output.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
                // Underline on (only if flagging)
                if flagging {
                    output.extend_from_slice(b"\x1b[4m");
                }
                // Character
                let mut buf = [0u8; 4];
                output.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                // Underline off (only if flagging)
                if flagging {
                    output.extend_from_slice(b"\x1b[24m");
                }
                output
            }
            Command::Backspace {
                row, col, width, ..
            } => {
                // Backspace overlay: show space at target position
                let mut output = Vec::new();
                // Move to position
                output.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
                // Underline on (only if flagging)
                if flagging {
                    output.extend_from_slice(b"\x1b[4m");
                }
                // Space(s) to cover the character
                for _ in 0..*width {
                    output.push(b' ');
                }
                // Underline off (only if flagging)
                if flagging {
                    output.extend_from_slice(b"\x1b[24m");
                }
                output
            }
            Command::CursorMove {
                from_row,
                from_col,
                direction,
            } => {
                // For cursor move, we just move the cursor (no underline)
                let (new_row, new_col) = match direction {
                    Direction::Left => (*from_row, from_col.saturating_sub(1)),
                    Direction::Right => (*from_row, from_col + 1),
                    Direction::Up => (from_row.saturating_sub(1), *from_col),
                    Direction::Down => (from_row + 1, *from_col),
                };
                format!("\x1b[{};{}H", new_row + 1, new_col + 1).into_bytes()
            }
        }
    }

    /// Undo the command - returns bytes to restore original state
    /// Uses the term to get current cell values (for rollback)
    pub fn undo<E>(&self, term: &Term<E>) -> Vec<u8> {
        match self {
            Command::Char {
                row, col, width, ..
            } => {
                let mut output = Vec::new();
                // Move to position
                output.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
                // Write emulator's current value (ground truth)
                let cell = &term.grid()[Line(*row as i32)][Column(*col)];
                if cell.c == '\0' || cell.c == ' ' {
                    for _ in 0..*width {
                        output.push(b' ');
                    }
                } else {
                    let mut buf = [0u8; 4];
                    output.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
                }
                output
            }
            Command::Backspace {
                row, col, width, ..
            } => {
                // Undo backspace overlay = restore server's current value (not original)
                let mut output = Vec::new();
                // Move to position
                output.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
                // Write emulator's current value (ground truth from server)
                let cell = &term.grid()[Line(*row as i32)][Column(*col)];
                if cell.c == '\0' || cell.c == ' ' {
                    for _ in 0..*width {
                        output.push(b' ');
                    }
                } else {
                    let mut buf = [0u8; 4];
                    output.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
                }
                output
            }
            Command::CursorMove {
                from_row, from_col, ..
            } => {
                // Undo cursor move = go back to original position
                format!("\x1b[{};{}H", from_row + 1, from_col + 1).into_bytes()
            }
        }
    }

    /// Get the display width of this command (for cursor positioning)
    /// Positive = moves cursor right, Negative = moves cursor left
    pub fn cursor_delta(&self) -> i32 {
        match self {
            Command::Char { width, .. } => *width as i32,
            Command::Backspace { width, .. } => -(*width as i32),
            Command::CursorMove { direction, .. } => match direction {
                Direction::Left => -1,
                Direction::Right => 1,
                Direction::Up | Direction::Down => 0,
            },
        }
    }

    /// Get the absolute width of this command
    pub fn width(&self) -> u8 {
        match self {
            Command::Char { width, .. } => *width,
            Command::Backspace { width, .. } => *width,
            Command::CursorMove { .. } => 1,
        }
    }

    /// Check if this is a Char command
    pub fn is_char(&self) -> bool {
        matches!(self, Command::Char { .. })
    }

    /// Get the character if this is a Char command
    pub fn as_char(&self) -> Option<(char, u8)> {
        match self {
            Command::Char { ch, width, .. } => Some((*ch, *width)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_char_execute_with_flag() {
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        let output = cmd.execute(true);
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("\x1b[1;1H")); // move to (0,0)
        assert!(output_str.contains("\x1b[4m")); // underline on
        assert!(output_str.contains('a'));
        assert!(output_str.contains("\x1b[24m")); // underline off
    }

    #[test]
    fn test_char_execute_without_flag() {
        let cmd = Command::char(0, 0, 'a', ' ', 1);
        let output = cmd.execute(false);
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("\x1b[1;1H")); // move to (0,0)
        assert!(!output_str.contains("\x1b[4m")); // no underline
        assert!(output_str.contains('a'));
    }

    #[test]
    fn test_char_execute_wide() {
        let cmd = Command::char(0, 0, '가', ' ', 2);
        let output = cmd.execute(true);
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains('가'));
    }

    #[test]
    fn test_backspace_execute() {
        // Backspace at col 2, original char 'c'
        let cmd = Command::backspace(0, 2, 'c', 1);
        let output = cmd.execute(true);
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("\x1b[1;3H")); // move to (0,2) - 1-indexed
        assert!(output_str.contains("\x1b[4m")); // underline on
        assert!(output_str.contains(' ')); // space to clear
    }

    #[test]
    fn test_cursor_delta() {
        assert_eq!(Command::char(0, 0, 'a', ' ', 1).cursor_delta(), 1);
        assert_eq!(Command::char(0, 0, '가', ' ', 2).cursor_delta(), 2);
        // Backspace moves cursor left
        assert_eq!(Command::backspace(0, 2, 'c', 1).cursor_delta(), -1);
        assert_eq!(
            Command::cursor_move(0, 5, Direction::Left).cursor_delta(),
            -1
        );
        assert_eq!(
            Command::cursor_move(0, 5, Direction::Right).cursor_delta(),
            1
        );
    }
}
