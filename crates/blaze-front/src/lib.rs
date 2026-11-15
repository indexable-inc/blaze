//! The front end's tasks under the keyed driver (docs/v2/plan/design/host.md,
//! docs/v2/plan/design/read-addressed.md): the `surface` and `body` of
//! `blaze_keyed::front::Front`, as two plain functions.
//!
//! The rule: a body is checked against records, never against a crate built in memory. Every
//! crate a body task reads, its own included, is read through that crate's `Surface` record,
//! mapped and used in place. Only a crate's own `Surface` task builds its scopes in memory.
//!
//! Records are read-addressed: each is written by `blaze_keyed::record::write_read`, filed at
//! the address of its key (what it is, from its own text) and its shape (every fact its work
//! asked, with the answer `Universe::answer` gave). Nothing here answers a fact any other way:
//! there is no `now` and no reuse path; a changed fact is a new address, an undone one the old.
//!
//! - [`facts`]: the one mapping from what the front end reads (blaze-item-hash's fact
//!   references) to fact ids, each by `record::fact` with its owner.
//! - [`surface`]: [`surface::surface`], `Front::surface`: a unit's `Surface` record; and
//!   [`surface::Opened`], opening surfaces into a snapshot.
//! - [`body`]: [`body::body`], `Front::body`: one body's check as a `Body` record, over a
//!   [`body::Check`] ([`body::front::Front`] over the ported front end).
//!
//! **The adapter.** The command implements `Front` with blaze-unitkey's keys and these two
//! functions; it lives in the command, not here, because `Front::key` is blaze-unitkey's and
//! blaze-front does not depend on it, and because the snapshots a task runs in (each unit's
//! crate built from text with its dependencies given from their surfaces, [`surface::Opened`])
//! are the command's state. Its two methods here are one line each:
//!
//! ```ignore
//! impl blaze_keyed::front::Front for Command {
//!     fn key<A: Allocator + Clone>(&self, unit: u32, frame: A) -> Result<Keyed<A>, TaskFailed> {
//!         self.unit_keys(unit, frame) // blaze-unitkey's `unit::key`, moved field for field
//!     }
//!     fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
//!         let at = self.snapshot(unit, universe)?; // `db`, `krate`: deps opened from their surfaces
//!         blaze_front::surface::surface(at.db(), at.krate(), key, universe, at.text_ids())
//!     }
//!     fn body(&self, unit: u32, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
//!         let at = self.snapshot(unit, universe)?; // every crate, the unit's own too, opened
//!         let find = |item| at.find(item); // the body blaze-unitkey's id names
//!         let check = blaze_front::body::front::Front { db: at.db(), traces: at.traces(), find: &find };
//!         blaze_front::body::body(&check, item, key, universe)
//!     }
//! }
//! ```
//!
//! The universe the adapter hands a unit's task holds, in `outside`, that unit's late reads:
//! `record::late_id` names no unit, and `env!("CARGO_PKG_VERSION")` is each crate's own, so
//! the same id must answer per asker (blaze-host's `Request::late` is one table for every
//! unit today, crates/blaze-host/src/host.rs:85 and :441). A surface whose expansions saw
//! another value than its unit's table answers fails ([`surface::LATE_MOVED`]).
//!
//! A body task's snapshot is built with read lists kept (`blaze_memo::Reads::Kept`) and its
//! item store installed by `hir_ty::body_store::install_trace(db,
//! Arc::new(blaze_item_hash::Bodies), build)` before any query runs; in a snapshot that keeps
//! no read lists every step's trace is refused as `Refusal::Unshareable`, so every body task
//! fails by that name. No check cache need be kept uninstalled: under a trace store the old
//! cache is not read (`check_cache::served_body` gives nothing when `body_store::tracing`).
//!
//! Nothing here does IO: records come in as bytes and go out as bytes.

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]

pub mod body;
pub mod facts;
pub mod surface;
