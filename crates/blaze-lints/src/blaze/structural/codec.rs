//! `hand_written_codec`: an encoder or decoder written field by field, the same code once per
//! field: an impl of a codec trait (`Persist`, `Encode`, `Decode`, `Serialize`,
//! `Deserialize`, `Codec`, `Record`), or a method named `encode`, `decode`, `persist`,
//! `restore`, `write_to`, `read_from`, `to_bytes`, `from_bytes`, `serialize` or `deserialize`,
//! whose body is [`Shape::Uniform`] or [`Shape::AlmostUniform`] over three fields or more.
//!
//! The format of a record is its structure: a codec written by hand is a second copy of the
//! field list, kept in step by hand, and the fields it treats differently are where formats
//! drift. The finding names them. The derived form today is `blaze_format::record!` (a record
//! declared once, its codec made from the declaration); the end state is one generic codec
//! over `for const f in fields_of::<T>()` (rules outputs-are-records, reflection-first). A
//! field it forgets is `impl_misses_field`'s. Advice, no fix: moving a type onto a derived
//! codec changes its bytes, which a persisted format's version must say.

use syntax::view::{AstNode as _, HasName as _};

use super::{Impl, listed, uniform::Shape};
use crate::{
    blaze::{Lint, std_mistakes::report},
    check::Cx,
};

const LINT: Lint = Lint::HandWrittenCodec;

/// The traits whose impls are codecs, by name.
const TRAITS: &[&str] =
    &["Persist", "Encode", "Decode", "Serialize", "Deserialize", "Codec", "Record"];

/// The methods that are an encoder or a decoder, by name.
const METHODS: &[&str] = &[
    "encode",
    "decode",
    "persist",
    "restore",
    "write_to",
    "read_from",
    "to_bytes",
    "from_bytes",
    "serialize",
    "deserialize",
];

/// The fewest fields a codec covers by one template to be reported.
const LEAST: usize = 3;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(LINT) {
        return;
    }
    for imp in impls {
        let codec_trait = imp.trait_name.is_some_and(|it| TRAITS.contains(&it));
        let found = imp.methods.iter().find(|it| {
            (codec_trait || METHODS.contains(&it.name))
                && matches!(it.analysis.shape, Shape::Uniform | Shape::AlmostUniform)
                && it.analysis.covered.len() >= LEAST
        });
        let Some(method) = found else { continue };
        let fields = imp.field_names();
        let analysis = &method.analysis;
        let message = format!(
            "`{}` of `{}` codes each of its {} fields with the same code, by hand",
            method.name,
            imp.said(),
            analysis.covered.len(),
        );
        let mut note = format!(
            "a record's format is its structure: declare it once and derive the codec \
             (`blaze_format::record!` today; one generic codec over `fields_of::<{}>()` in the \
             end state), so a field added later is coded with no edit",
            imp.type_name,
        );
        if !analysis.odd.is_empty() {
            note.push_str(&format!(
                "; {} {} coded differently from the rest: give its type its own codec, or say \
                 there why",
                listed(&fields, &analysis.odd),
                if analysis.odd.len() == 1 { "is" } else { "are" },
            ));
        }
        let range = match method.node.name() {
            Some(it) => it.syntax().text_range(),
            None => method.node.syntax().text_range(),
        };
        report(cx, LINT, method.node.syntax(), range, message, &note, None);
    }
}
