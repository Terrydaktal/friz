# friz

An ultrafast, lightweight fuzzy finder built with Rust and the SIMD-accelerated `frizbee` library. Designed as a drop-in replacement for `fzf` in simple pipe workflows.

## Features
- **SIMD-accelerated:** Uses `frizbee` for high-performance fuzzy matching based on the Smith-Waterman algorithm.
- **Lightweight:** Minimal dependencies, focused on speed.
- **TTY-aware:** Reads from stdin for the list, but uses `/dev/tty` for interactive input, making it perfect for shell pipes.
- **Flexible UI:** Supports both Bottom-Up (default) and Top-Down (`--reverse`) layouts, with customizable height and headers.
- **Cancellable Search:** Sub-millisecond search cancellation via chunked processing (20,000 lines/chunk).
- **Progressive Rendering:** 60fps streaming updates for large datasets, providing instantaneous visual feedback.
- **High Performance:** Uses `jemalloc` for efficient memory management and a non-blocking architecture that decouples rendering from data ingestion.

## Keyboard Shortcuts
- **Enter**: Select the current item.
- **Esc / Ctrl+C**: Exit without selecting.
- **Up / Ctrl+P**: Move selection up.
- **Down / Ctrl+N**: Move selection down.
- **Ctrl+W / Ctrl+Backspace**: Delete the last word (back to `/` or space).
- **Backspace**: Delete the last character.

## Project Structure

```text
.
├── .gitignore          # Git ignore rules
├── Cargo.lock          # Locked dependencies
├── Cargo.toml          # Rust project configuration
├── README.md           # Project documentation
└── src/
    └── main.rs         # Core logic: TUI, matching, and selection
```

- `src/main.rs`: Contains the entire application logic including CLI argument parsing, background stdin reading, search narrowing, and the cross-platform TUI renderer.

## Installation

```bash
cargo build --release
sudo cp target/release/friz /usr/local/bin/
```

## Usage

Pipe any list of strings to `friz`:

```bash
ls | friz
find . -type f | friz
```

### Options
- `--reverse`: Use Top-Down layout.
- `--height <N>%`: Set the TUI height as a percentage of terminal height.
- `--header "<text>"`: Add a header string above/below the search prompt.

### Keybindings (Zsh)

Add the following to your `.zshrc` to bind `friz` to `Ctrl+T` (find file and insert into command line):

```zsh
friz-file-widget() {
  local selected=$(find . -maxdepth 4 | friz)
  if [ -n "$selected" ]; then
    LBUFFER="${LBUFFER}${selected}"
  fi
  zle reset-prompt
}
zle -N friz-file-widget
bindkey '^T' friz-file-widget
```

## Implementation Details

`friz` leverages several advanced techniques for performance:
1. **Hyper-Optimized Reading:** Uses a 1MB `BufReader` and a reusable `String` buffer to minimize system I/O calls and heap allocations.
2. **Search Narrowing:** If a new query starts with the previous query, it only searches within the previous results.
3. **Non-Blocking Render Loop:** State is cloned and the lock is dropped before terminal I/O, ensuring data ingestion and search are never blocked by rendering.
4. **Jemalloc:** Uses `tikv-jemallocator` for better performance under high allocation churn.
5. **Direct TTY Access:** Opens `/dev/tty` for input/output to allow usage in shell pipelines where `stdin` is already consumed.
