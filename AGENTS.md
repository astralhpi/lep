# AGENTS.md

## Project Overview

**lep** (Local Echo Proxy) - Terminal proxy providing instant typing feedback on high-latency networks.

Implements mosh's predictive local echo on top of SSH. No server installation required.

## Goals

- mosh-level input prediction (characters, backspace)
- Works with existing SSH connections without server changes
- Wide character support (Korean, CJK, emoji)
- Full-screen app support (vim/nvim, less, etc.)

## Tech Stack

- **Language**: Rust
- **Core Libraries**:
  - `alacritty_terminal`: Terminal emulator from Alacritty project
  - `nix`: POSIX API (PTY, signals, etc.)
  - `unicode-width`: Character width detection

## Architecture: Passthrough + Overlay

```
Server output -> Passthrough to terminal (as-is)
             -> Feed to emulator (state tracking)

User input -> Show prediction overlay -> Restore cursor
           -> Forward to server
           
Server confirms -> If matches, prediction naturally overwritten
                -> If mismatch, restore original char, show server output
```

**Key insight**: No diff calculation. Server output passes through directly.

**Emulator role**: 
- Track server state (screen content, cursor position)
- Provide original chars for rollback on mismatch
- Does NOT render to screen (passthrough handles that)

## Directory Structure

```
lep/
├── AGENTS.md          # This file (AI assistant instructions)
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs         # Module exports
│   ├── main.rs        # CLI and proxy loop
│   ├── command.rs     # Prediction commands (Char, Backspace)
│   ├── emulator.rs    # Terminal state + adaptive prediction
│   └── pty.rs         # PTY handling
```

## Core Modules

### command.rs
- `Command` enum: `Char`, `Backspace`, `CursorMove`
- `execute(flagging)`: Generate overlay bytes
- `undo(term)`: Generate rollback bytes
- `cursor_delta()`: How much cursor moves

### emulator.rs
- `Emulator`: Terminal state tracking with alacritty_terminal
- `feed_and_verify()`: Process server output, verify predictions
- Adaptive prediction (mosh-style SRTT-based)
- `should_predict()`, `should_flag()`: Adaptive decision methods

### pty.rs
- `PtyMaster`: Spawn and communicate with child process
- Window resize handling
- Clean child process termination

### main.rs
- CLI parsing (`-p always|auto|never`)
- Proxy loop: stdin -> PTY, PTY -> stdout
- `InputHandler`: UTF-8 input processing
- `ServerOutputBuffer`: Incomplete UTF-8 handling

## Development Principles

1. **TDD**: Write tests first, then implement
2. **E2E tests preferred**: Real PTY integration tests when possible
3. **Complete features**: Finish one thing before starting another
4. **Keep it simple**: Only implement what's needed
5. **Fast feedback**: Small commits, frequent testing

## Development Guide

### Build
```bash
cargo build --release
```

### Test
```bash
cargo test
```

### Run
```bash
./target/release/lep ssh user@host
./target/release/lep -p always ssh user@host  # Force prediction
./target/release/lep -p never ssh user@host   # Passthrough only
```

### Debug
```bash
./target/release/lep -vvv --log /tmp/lep.log ssh user@host
```
