# friz

`friz` is a fast interactive line selector for shell pipelines.
It reads candidate lines from `stdin`, uses `/dev/tty` for interaction, and prints the selected line to `stdout`.

## Current Behavior
- Case-insensitive, multi-term substring matching.
- Query is split by whitespace; each term must be present for a line to match.
- Progressive ingestion of large inputs in `20,000`-line chunks.
- Parallel search across chunks with `rayon`.
- In-place terminal rendering with both default and `--reverse` layouts.
- `jemalloc` as global allocator for high-churn workloads.

## Project Structure

```text
.
├── .gitignore
├── Cargo.lock
├── Cargo.toml
├── README.md
└── src
    └── main.rs
```

- `src/main.rs`: Full application pipeline (arg parsing, stdin ingestion, background search, TUI rendering, key handling, and selection output).

## Installation

```bash
cargo build --release
cp target/release/friz ~/.local/bin/friz
```

## Inputs and Outputs
- Input: newline-delimited candidates from `stdin`.
- Interactive control: keyboard events from `/dev/tty`.
- Output on `Enter`: selected line written to `stdout`.
- Output on `Esc` / `Ctrl+C`: no selection printed.

## Usage

```bash
find . -type f | friz
find . -maxdepth 4 | friz --reverse --height 40% --header="Select path"
```

## CLI Options
- `--reverse`: top-down layout (prompt first, results below).
- `--height <N>` or `--height=<N>`: list height as percent of terminal height (`40` and `40%` are both accepted).
- `--header=<text>`: optional header line.

Note: header parsing is currently `--header=<text>` form.

## Keyboard Shortcuts
- `Enter`: accept selection.
- `Esc` or `Ctrl+C`: cancel.
- `Up` or `Ctrl+P`: move selection up.
- `Down` or `Ctrl+N`: move selection down.
- `Backspace`: delete one character.
- `Ctrl+W`, `Ctrl+Backspace`, or `Ctrl+H`: delete previous segment up to `/` or space.

## Pipeline Order
1. Parse CLI flags.
2. Open `/dev/tty` for interactive input/output while preserving piped `stdin`.
3. Reader thread streams input lines into chunk storage.
4. Search thread recomputes matches when query/data versions change.
5. Render loop updates the TUI when state version changes.
6. On `Enter`, print selected line and clean up terminal state.

## Implementation Notes
- Shared state uses fine-grained `Mutex` + `AtomicUsize` fields to reduce lock contention.
- Search aborts stale work using atomic query version checks.
- Result ordering is deterministic: score then source index.
- Rendering clears previous frame by tracked height to avoid progressive frame stacking.
