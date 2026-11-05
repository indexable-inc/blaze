//! The slot for Codex's adapter: `codex app-server` (JSON-RPC over stdio: threads, turns, streaming deltas, interrupt)
//! read into the event model in `chat/mod.rs`, as `claude.rs` reads Claude's stream-json. Not written yet; Codex
//! sessions run in a PTY until it is.
