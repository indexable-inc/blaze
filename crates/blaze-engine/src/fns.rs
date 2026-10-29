//! The functions each build puts in the program, said as [`Event::Functions`] records beside the
//! process's socket as the codegen puts them in the object ([`hir_ty_codegen::watch`]): what
//! the build page draws (https://github.com/indexable-inc/blaze/pull/1341).

use std::sync::Arc;

use blaze_protocol::events::{Event, Function, How};
use hir_ty_codegen::watch;

/// Has the codegen say the functions of every object from now on. A process says them
/// only once it has a socket to say them beside.
pub(crate) fn said() {
    watch::watch(Some(Arc::new(|object, whole, placed: Vec<watch::Placed>| {
        let functions = placed.into_iter().map(function).collect();
        blaze_cargo::events::say(Event::Functions { object, whole, functions });
    })));
}

/// A build of the program begins.
pub(crate) fn building() {
    watch::begin();
}

fn function(placed: watch::Placed) -> Function {
    let watch::Placed { id, key, krate, name, calls, how } = placed;
    let how = match how {
        watch::How::Compiled { ns } => How::Compiled { ns },
        watch::How::Unit => How::Unit,
        watch::How::Unchanged => How::Unchanged,
        watch::How::Stored => How::Stored,
    };
    Function { id, key, krate, name, calls, how }
}
