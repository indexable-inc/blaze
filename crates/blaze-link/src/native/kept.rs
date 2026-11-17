//! The native resolution and inputs as parts of a kept state ([`crate::keep`]).
//!
//! Everything a stage reads of the resolution is kept but its graph and the marks over the
//! graph's numbering: an opened resolution has no graph, so the first stage after an open
//! builds it again, as it does for a changed input (the stage's `resolve`, its rebuild). Its
//! inputs and names are the state's, shared.

#![forbid(unsafe_code)]

use std::alloc::Allocator;
use std::sync::Arc;

use blaze_ids::ContentId;

use crate::keep::{Bad, In, Kept, Out};
use crate::model::{Formats, Writer};
use crate::state::Input;
use crate::NativeKind;

use super::{CowFlags, Marks, Refs, Resolved, check_file};

impl Kept for Refs {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let Refs { counts, of, lists, names, listed } = self;
        counts.put(out);
        of.put(out);
        lists.put(out);
        names.put(out);
        listed.0.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<Refs, Bad> {
        let counts = Kept::take(input)?;
        let of = Kept::take(input)?;
        let lists = Kept::take(input)?;
        let names = Kept::take(input)?;
        Ok(Refs { counts, of, lists, names, listed: CowFlags(Kept::take(input)?) })
    }
}

impl Resolved {
    pub(crate) fn keep<A: Allocator>(&self, out: &mut Out<A>) {
        let Resolved {
            imports,
            import_syms,
            libraries,
            library_orders,
            exports,
            stale,
            refs,
            pieces,
            removals,
            // The state's own, shared: `open`'s caller sets them from the state.
            inputs: _,
            names: _,
            // Empty: `Image::keep` refuses a wasm import.
            wasm: _,
            // Built again by the first stage after an open.
            graph: _,
            marks: _,
            outcomes,
            named,
            defined,
            library_index,
            commons,
        } = self;
        imports.put(out);
        import_syms.put(out);
        libraries.put(out);
        library_orders.put(out);
        exports.put(out);
        stale.put(out);
        refs.put(out);
        pieces.put(out);
        removals.put(out);
        outcomes.put(out);
        named.put(out);
        defined.put(out);
        library_index.put(out);
        commons.put(out);
    }

    /// The resolution `keep` wrote, but its inputs and names: the state's, shared, which the
    /// caller sets in place of the empty ones here.
    pub(crate) fn open(input: &mut In<'_>) -> Result<Resolved, Bad> {
        let imports = Kept::take(input)?;
        let import_syms = Kept::take(input)?;
        let libraries = Kept::take(input)?;
        let library_orders = Kept::take(input)?;
        let exports = Kept::take(input)?;
        let stale = Kept::take(input)?;
        let refs = Kept::take(input)?;
        let pieces = Kept::take(input)?;
        let removals = Kept::take(input)?;
        let outcomes = Kept::take(input)?;
        let named = Kept::take(input)?;
        let defined = Kept::take(input)?;
        let library_index = Kept::take(input)?;
        let commons = Kept::take(input)?;
        Ok(Resolved {
            imports,
            import_syms,
            libraries,
            library_orders,
            exports,
            stale,
            refs,
            pieces,
            removals,
            inputs: Arc::default(),
            names: Arc::default(),
            wasm: Arc::default(),
            graph: None,
            marks: Marks::default(),
            outcomes,
            named,
            defined,
            library_index,
            commons,
        })
    }
}

/// A native input as `State::keep_section` wrote it, its file parsed again from its bytes
/// and checked as a stage checks a new one.
pub(crate) fn open_input(input: &mut In<'_>, writer: &Formats) -> Result<Input, Bad> {
    let id = ContentId::take(input)?;
    let name: Arc<str> = Kept::take(input)?;
    let kind = NativeKind::take(input)?;
    let order = input.u32()?;
    let bytes: Arc<[u8]> = Arc::from(input.bytes()?);
    let file = writer.read(&name, &bytes, &kind).map_err(|it| Bad::Text(format!("{name}: {it}")))?;
    check_file(&file, bytes.len()).map_err(|it| Bad::Text(format!("{name}: {it}")))?;
    Ok(Input { id, name, kind, order, bytes, file: Arc::new(file) })
}
