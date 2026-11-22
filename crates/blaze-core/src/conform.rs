//! A model held against what it models (D167 part c): one conformance suite per outside
//! service, its cases written once over the service's trait and run unchanged on the model
//! (in ordinary tests, `unit`) and on the real service (a scratch directory under the test's
//! own temporary folder, `process`; no network, nothing outside that folder).
//!
//! A case answers one typed value, what it saw: the effect's answer as a kind (done, refused,
//! a file of this length, a directory, not there; never an error's text, which differs by
//! system) and the state after it, looked at through the same trait. Its expectation is one
//! value too, the same everywhere or one per platform where systems legitimately differ (a
//! disk that folds case and one that does not); a case never skips its assert.
//!
//! The rule the suite holds (docs/v2/plan/design/sim.md, "A model is held against what it
//! models"): a model may be stricter than the real service, never more permissive. Where the
//! model refuses what a real disk does (a name spelled two ways), the model's expectation is
//! its own named value; where a real disk refuses what the model did, the model is wrong and
//! is fixed. Every bug found on the real service is added here as a case, which adds it to
//! both by construction.
//!
//! - [`fs`]: the file system, over [`crate::Driver`]'s file effects: [`crate::sim::Sim`]
//!   against this host's native driver.
//! - The journal's suite is in `fsevents.rs`'s tests (`JournalCase`), over a stopped and
//!   resumed journal: fseventsd itself against the model of its store.

mod fs;
