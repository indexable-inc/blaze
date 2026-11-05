//! The jump table's execution log in the run process (fastcompile-ident-select 9): the dev
//! runtime's (`blaze_inproc_rt::live::log`), which every dev executable carries whatever it
//! depends on, since a test binary's recording fork server imports its `__blaze_exec_slots` and
//! `__blaze_exec_arm`. The switch registers its tables there ([`TABLES`]) and stores a table
//! through the same [`Header`].

pub use blaze_inproc_rt::live::log::{ARMED, NO_LOG, NOT_A_RECORD, OTHER_SLOTS, SHORT, publish};
pub(crate) use blaze_inproc_rt::live::log::{Header, TABLES, Tables};
