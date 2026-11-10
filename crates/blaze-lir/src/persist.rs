//! A kept LIR: one blaze-format buffer whose root ([`LirRoot`]) holds every column of the
//! function as a [`Slice`], the same bytes a [`Func`] holds in memory. A consumer reads it in
//! place ([`read`] gives a [`View`]) with no decoding into another structure.
//!
//! The schema hash covers the root's every field, every table row's layout, LIR's
//! [`crate::VERSION`] and the operation table ([`crate::op::table_code`]), so a LIR of another
//! encoding reads as a schema mismatch, never as another function. [`read`] checks the buffer
//! as blaze-format does, then every tag, length and index of the function
//! ([`crate::verify::structure`]), so a reader may index the view it gets without checking.
//!
//! Whether to keep LIR in the store, and for which builds, is decided after the switch to LIR,
//! once measured (docs/v2/plan/design/lir.md, open question 3): this is the record that is kept
//! then, and what the round-trip tests hold to its bytes now.

use std::{alloc::Global, mem::offset_of};

use blaze_format::{FormatError, Part, Persist, Placed, Record, SchemaHash, Slice, Writer, open_as, schema_of};

use blaze_format::SpanRef;

use crate::func::{
    AsmOperandRec, AsmPieceRec, AsmRec, CompoundRec, DataRec, DataRelocRec, InlinedRec, IntrinsicRec,
    MemAttrsRec, ParamFactsRec, ParamRec, RangeRec, RefRec, SigRec, SlotRec, VarRec,
};
use crate::{
    func::{Func, Head, View, columns},
    op::{Op, table_code},
    verify::{self, VerifyError},
};

macro_rules! root {
    ($($(#[$meta:meta])* $name:ident: $ty:ty,)*) => {
        blaze_format::record! {
            /// A kept LIR function: its head, then each column.
            #[derive(Clone, Copy)]
            pub struct LirRoot: 1 {
                pub head: Head,
                $( pub $name: Slice<$ty>, )*
            }
        }

        /// Writes `view`'s columns, then the root naming them; returns where the root is.
        fn write_columns(view: &View<'_>, out: &mut Writer) -> u64 {
            $( let $name = Placed::push(out, view.$name); )*
            let at = out.next::<LirRoot>();
            let root = LirRoot {
                head: view.head,
                $( $name: $name.slice(at + offset_of!(LirRoot, $name) as u64), )*
            };
            let written = out.push(&root);
            debug_assert_eq!(written, at);
            at
        }

        /// The columns `root` names in `buffer`, each checked to lie in it.
        fn read_columns<'a>(root: &'a LirRoot, buffer: &'a [u8]) -> Result<View<'a>, FormatError> {
            Ok(View { head: root.head, $( $name: root.$name.follow(buffer)?, )* })
        }
    };
}

columns!(root);

const OPS: [u64; Op::COUNT] = table_code();

/// The schema of a LIR buffer.
pub const SCHEMA: SchemaHash = {
    let mut parts = [Part::U64(0); Op::COUNT + 4];
    parts[0] = Part::Str("blaze_lir::Func");
    parts[1] = Part::U64(crate::VERSION);
    parts[2] = Part::Schema(<LirRoot as Record>::SCHEMA);
    parts[3] = Part::Schema(<SpanRef as Record>::SCHEMA);
    let mut i = 0;
    while i < Op::COUNT {
        parts[4 + i] = Part::U64(OPS[i]);
        i += 1;
    }
    schema_of(&parts)
};

/// Why bytes are not a kept LIR.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// Not a buffer of this schema, or a record that leaves it.
    Format(FormatError),
    /// A buffer of this schema whose columns are not a function: `structure`'s refusal.
    Structure(VerifyError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Format(error) => write!(f, "not a kept LIR: {error}"),
            ReadError::Structure(error) => write!(f, "a kept LIR that is not a function: {error}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// `view` written into `out`, a writer of [`SCHEMA`]: where its root is.
pub fn write(view: &View<'_>, out: &mut Writer) -> u64 {
    write_columns(view, out)
}

/// `view` as a buffer of its own.
pub fn to_bytes(view: &View<'_>) -> Vec<u8> {
    let mut out = Writer::new(SCHEMA);
    let root = write(view, &mut out);
    out.finish(root)
}

/// The function in `buffer`, read in place: its schema, its records' bounds, then every tag,
/// length and index of its columns.
pub fn read(buffer: &[u8]) -> Result<View<'_>, ReadError> {
    let root = open_as::<LirRoot>(buffer, SCHEMA).map_err(ReadError::Format)?;
    let view = read_columns(root, buffer).map_err(ReadError::Format)?;
    verify::structure(view).map_err(ReadError::Structure)?;
    Ok(view)
}

/// A function on the global heap, as the store keeps one: its record is [`to_bytes`] of it.
impl Persist for Func<Global> {
    const SCHEMA: SchemaHash = SCHEMA;
    type Ref<'a> = View<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        write(&self.view(), out)
    }

    fn read(buffer: &[u8]) -> Result<View<'_>, FormatError> {
        let root = open_as::<LirRoot>(buffer, SCHEMA)?;
        let view = read_columns(root, buffer)?;
        verify::structure(view).map_err(|_| FormatError::Invalid {
            at: 0,
            what: "a LIR whose columns are not a function (see blaze_lir::persist::read)",
        })?;
        Ok(view)
    }
}
