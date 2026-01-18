# lep

**Local Echo Proxy** - mosh-style predictive local echo for SSH without server-side installation.

## What is this?

When you type over a high-latency SSH connection, there's a noticeable delay before characters appear. [mosh](https://mosh.org/) solves this with "predictive local echo" - showing your keystrokes immediately while the server catches up.

**lep** brings this same experience to regular SSH connections, without requiring anything installed on the server.

## Features

- **Instant feedback**: Characters appear immediately, even on 500ms+ latency connections
- **Adaptive prediction**: Automatically enables prediction only when latency is detected (SRTT > 30ms)
- **Wide character support**: Korean, Chinese, Japanese, emoji - all work correctly
- **vim/less aware**: Automatically disables prediction in full-screen applications
- **Zero server requirements**: Works with any SSH server, no installation needed

## Installation

### From source

```bash
git clone https://github.com/anomalyco/lep
cd lep
cargo build --release
sudo cp target/release/lep /usr/local/bin/
```

### Requirements

- Rust 1.70+
- macOS or Linux

## Usage

```bash
# Basic usage - wrap your SSH command
lep ssh user@remote-host

# Force prediction always (even in vim)
lep -p always ssh user@host

# Disable prediction (pure passthrough)
lep -p never ssh user@host

# Debug mode
lep -vvv --log /tmp/lep.log ssh user@host
```

## How it works

```
┌─────────────────────────────────────────────────────────────┐
│                        Terminal                             │
└─────────────────────────────────────────────────────────────┘
                              ↑
                              │ Server output (passthrough)
                              │ + Prediction overlay
                              │
┌─────────────────────────────────────────────────────────────┐
│                          lep                                │
│  ┌─────────────┐    ┌──────────────┐    ┌───────────────┐  │
│  │   Input     │───→│   Emulator   │───→│   Verify &    │  │
│  │  Handler    │    │ (state track)│    │   Rollback    │  │
│  └─────────────┘    └──────────────┘    └───────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              ↕
                      SSH (stdin/stdout)
                              ↕
┌─────────────────────────────────────────────────────────────┐
│                     Remote Server                           │
└─────────────────────────────────────────────────────────────┘
```

1. **You type** → lep shows the character immediately (with underline if uncertain)
2. **Server responds** → lep verifies the prediction
3. **Match** → Prediction is silently confirmed
4. **Mismatch** → Prediction is rolled back, server output shown

## Prediction Modes

| Mode | Description |
|------|-------------|
| `auto` (default) | Adaptive prediction based on measured latency |
| `always` | Always predict, even in vim/less |
| `never` | Pure passthrough, no prediction |

In `auto` mode, lep measures round-trip time (SRTT) and:
- Enables prediction when SRTT > 30ms
- Disables prediction when SRTT < 20ms
- Shows underline when SRTT > 80ms (uncertain predictions)

## Options

| Option | Description |
|--------|-------------|
| `-p, --predict <MODE>` | Prediction mode: `always`, `auto`, `never` |
| `-v, --verbose` | Increase verbosity (-v, -vv, -vvv) |
| `--log <FILE>` | Write logs to file |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

## Comparison with mosh

| Feature | mosh | lep |
|---------|------|-----|
| Server installation | Required | Not required |
| Protocol | UDP (custom) | SSH (TCP) |
| Predictive echo | Yes | Yes |
| Roaming | Yes | No |
| Works through NAT | Sometimes | Always (uses SSH) |

## Limitations

- No roaming support (unlike mosh)
- Prediction is character-by-character only
- Arrow key prediction not yet implemented

## Development

```bash
# Build
cargo build --release

# Test (41 unit tests)
cargo test

# Debug run
cargo run -- -vvv --log /tmp/lep.log ssh user@host
```

## License

MIT

## Credits

Inspired by [mosh](https://mosh.org/) by Keith Winstein.

Uses [alacritty_terminal](https://github.com/alacritty/alacritty) for terminal emulation.
