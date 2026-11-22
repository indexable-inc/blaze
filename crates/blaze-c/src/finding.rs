//! A finding of an import (rule 81): a declaration of the header that is not in the rendered
//! module, or not all of it, with its cause and an item-relative place. No level: whether a
//! finding is advice or an error is the reader's (the expansion reports each at the import).

use crate::model::{DeclKind, Unexpressed};
use crate::view::DeclsView;
use crate::NONE;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Finding {
    pub kind: DeclKind,
    /// The declaration's name in the record ([`NONE`] for an anonymous one).
    pub name: u32,
    /// Its namespace in the record ([`NONE`]: the global one).
    pub ns: u32,
    pub cause: Unexpressed,
    /// The header, by its row in the record's header column.
    pub header: u32,
    /// The declaration's bytes in its header.
    pub at: u32,
    pub len: u32,
    /// The field or parameter the finding is about, by index in its declaration; [`NONE`] for
    /// the whole declaration.
    pub part: u32,
}

impl Finding {
    /// The declaration's name, `None` for an anonymous one.
    pub fn name<'b>(&self, view: &DeclsView<'b>) -> Option<&'b str> {
        view.opt_name(self.name)
    }

    pub fn whole(&self) -> bool {
        self.part == NONE
    }
}
