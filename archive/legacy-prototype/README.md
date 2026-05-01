This directory preserves the pre-workspace prototype that originally lived in the
repo root `src/` tree.

It is not part of the Cargo workspace, is not built by `cargo build --workspace`,
and should not be used as the current implementation reference.

The active codebase lives in:

- `crates/voicersd`
- `crates/voicers`
- `crates/voicers-core`

The archived files remain here only for historical reference while the daemon/TUI
workspace continues to evolve.
