//! A checked body's read list and needs, filed out of its [`Checked`].
//!
//! A read list exists to be WRITTEN with the body's record, so a later run can check the record
//! against today's facts; nothing that keeps the body's result in the same run reads it again.
//! [`Checked::file_out`] moves the list and the needs out of the record, leaving both empty and
//! holding no memory, and hands them to the one caller that files them (writes them, or reduces
//! them to a digest). The value that lives on then holds no read list: 16,317 bodies of a hello
//! world held 254.5 MB of lists until the program was made (port27/hello-1116/TABLES-NOTES.md,
//! "Peak attribution", holder 4).
//!
//! The end state, which needs a cross-crate batch (blaze-typeck, blaze-build's closure record,
//! blaze-real's census, blaze-ladder): `Checked` without the two fields and the check's outcome
//! returning `Filed` beside it, so keeping a list by accident does not type-check.

use blaze_own::Soa;

use crate::columns::Checked;
use crate::world::{Needs, Question, ReadList};

/// What a check read, and what to fetch next time: moved out of a [`Checked`] by
/// [`Checked::file_out`] for the caller that writes it with the body's record.
pub struct Filed<'a> {
    pub reads: ReadList<'a>,
    pub needs: Needs<'a>,
}

impl<'a> Filed<'a> {
    /// The bytes the list and the needs hold, by capacity: what keeping them costs.
    pub fn live_bytes(&self) -> usize {
        read_list_bytes(&self.reads) + needs_bytes(&self.needs)
    }
}

/// The bytes `list` holds, by capacity: each row a question and its answer's 8-byte hash.
fn read_list_bytes(list: &ReadList<'_>) -> usize {
    list.rows.capacity() * (core::mem::size_of::<Question>() + core::mem::size_of::<u64>())
}

/// The bytes `needs` holds, by capacity.
fn needs_bytes(needs: &Needs<'_>) -> usize {
    (needs.questions.capacity() + needs.missing.capacity()) * core::mem::size_of::<Question>()
}

impl<'a> Checked<'a> {
    /// The read list and the needs moved out, for the one caller that files them; the record
    /// keeps an empty list and empty needs, which allocate nothing. Called once, where the
    /// check finished: a second call returns empty ones.
    pub fn file_out(&mut self) -> Filed<'a> {
        let alloc = *self.reads.rows.allocator();
        let reads = core::mem::replace(&mut self.reads, ReadList { rows: Soa::new_in(alloc) });
        let needs = core::mem::replace(&mut self.needs, Needs::new_in(alloc));
        Filed { reads, needs }
    }

    /// The bytes this record's read list and needs still hold, by capacity: 0 once filed out.
    /// What a test counts to show no kept record holds a list.
    pub fn read_list_bytes(&self) -> usize {
        read_list_bytes(&self.reads) + needs_bytes(&self.needs)
    }
}
