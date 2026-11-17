//! A text whose bytes name files (`concat!` with `file!()`), as code refers to it and as the
//! link composes it: the one place both sides read the recipe's layout.
//!
//! The code generator makes the text's recipe ([`recipe`]): a read-only datum of the function
//! (align 1) whose bytes are the text's literal parts in order with a hole of [`HOLE`] zero
//! bytes where each file's name goes, and whose relocations are one per hole, at its start,
//! [`RelocKind::Abs64`] with addend 0, to the [`Target::Position`] of the span whose file it
//! names. The code refers to the text by [`Target::Text`] of the recipe's ID: the address of a
//! record of a `&str` (pointer, then length: 16 bytes, 8-aligned; on wasm32 8 bytes,
//! 4-aligned), which the code loads as it loads a `Location`'s file name.
//!
//! The link ([`compose`]) writes the text: the recipe's bytes, each hole replaced by the name
//! of the file of its span from the span side table (as it fills a `Location`'s), and the
//! record pointing at them. The recipe itself is not placed: nothing but a [`Target::Text`]
//! refers to it.
//!
//! Why this holds nothing of the machine (rules 25, 71): the recipe's bytes are the program's
//! literals, its holes' targets are item-relative spans in a template (made relative and
//! absolute with every other position, `template`'s `relative` and `absolute`), and a file's
//! name exists only in the link's output.
//!
//! Floor: one pass over the recipe's bytes and one name lookup per hole; [`compose`] copies
//! each byte of the output once.

use std::alloc::{Allocator, Global};

use blaze_format::SpanRef;

use crate::{Data, RelocKind, Relocation, Relocs, RelocsView, Target};

/// The bytes a file's name stands in for in a recipe: one 64-bit relocation's field.
pub const HOLE: usize = 8;

/// One part of a text, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextPart<'a> {
    /// Literal bytes, as the program wrote them.
    Bytes(&'a [u8]),
    /// The name of the file of the span, as rustc reports `file!()`.
    FileOf(SpanRef),
}

/// Why a recipe does not compose, by the rule its layout breaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextError<E> {
    /// A relocation that is not a hole: not `Abs64`, a nonzero addend, or no position.
    NotAHole { at: u32 },
    /// Two holes that overlap, out of order, or one past the recipe's end.
    HoleOutOfPlace { at: u32 },
    /// A recipe past 4 GiB.
    TooLong,
    /// The span's file has no name: the link's own error.
    NoFile(E),
}

/// The recipe of the text of `parts`: its datum ([`Data::new`], read-only, align 1), whose ID
/// [`Target::Text`] names.
pub fn recipe(parts: &[TextPart<'_>]) -> Result<Data, TextError<core::convert::Infallible>> {
    let len = parts
        .iter()
        .map(|it| match it {
            TextPart::Bytes(bytes) => bytes.len(),
            TextPart::FileOf(_) => HOLE,
        })
        .sum::<usize>();
    let mut bytes = Vec::with_capacity_in(len, Global);
    let mut holes = Vec::new_in(Global);
    for part in parts {
        match part {
            TextPart::Bytes(literal) => bytes.extend_from_slice(literal),
            TextPart::FileOf(span) => {
                let at = u32::try_from(bytes.len()).map_err(|_| TextError::TooLong)?;
                holes.push(Relocation {
                    at,
                    kind: RelocKind::Abs64,
                    target: Target::Position(*span),
                    addend: 0,
                });
                bytes.extend_from_slice(&[0; HOLE]);
            }
        }
    }
    u32::try_from(bytes.len()).map_err(|_| TextError::TooLong)?;
    let relocs: Relocs = holes.into_iter().collect();
    Ok(Data::new(1, bytes, relocs))
}

/// The text of the recipe `bytes` with relocations `relocs` (a [`Target::Text`]'s datum),
/// appended to `out`: each literal byte as it is, each hole the name `file_of` gives its
/// span. Refused by name where the recipe is not one [`recipe`] makes.
pub fn compose<'n, E, A: Allocator>(
    bytes: &[u8],
    relocs: RelocsView<'_, Target>,
    mut file_of: impl FnMut(SpanRef) -> Result<&'n str, E>,
    out: &mut Vec<u8, A>,
) -> Result<(), TextError<E>> {
    let mut from = 0usize;
    for hole in relocs.iter() {
        let at = hole.at as usize;
        let span = match (hole.kind, hole.addend, hole.target) {
            (RelocKind::Abs64, 0, Target::Position(span)) => *span,
            _ => return Err(TextError::NotAHole { at: hole.at }),
        };
        if at < from || at.checked_add(HOLE).is_none_or(|end| end > bytes.len()) {
            return Err(TextError::HoleOutOfPlace { at: hole.at });
        }
        out.extend_from_slice(&bytes[from..at]);
        out.extend_from_slice(file_of(span).map_err(TextError::NoFile)?.as_bytes());
        from = at + HOLE;
    }
    out.extend_from_slice(&bytes[from..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_ids::ContentId;

    use super::*;

    fn span(index: u32) -> SpanRef {
        SpanRef { anchor: ContentId([7; 32]), index }
    }

    fn composed(recipe: &Data) -> Result<Vec<u8>, TextError<&'static str>> {
        let mut out = Vec::new_in(Global);
        let names = |span: SpanRef| match span.index {
            0 => Ok("src/main.rs"),
            1 => Ok("src/lib.rs"),
            _ => Err("no such span"),
        };
        compose(&recipe.bytes, recipe.relocs.view(), names, &mut out)?;
        Ok(out)
    }

    /// The invariant: a recipe composes to its literals with each file's name where its
    /// `file!()` stood, whatever bytes the literals hold (a literal of eight zero bytes is no
    /// hole: holes are the relocations). The controls: the same bytes with no relocations
    /// compose to the bytes alone, so the names came from the relocations; and a hole whose
    /// relocation has an addend is refused by name.
    #[test]
    fn a_recipe_composes_to_its_literals_and_the_files_named() {
        let parts = [
            TextPart::Bytes(b"at "),
            TextPart::FileOf(span(0)),
            TextPart::Bytes(&[0; HOLE]),
            TextPart::FileOf(span(1)),
            TextPart::Bytes(b"!"),
        ];
        let made = recipe(&parts).expect("a recipe");
        let mut want = b"at src/main.rs".to_vec();
        want.extend_from_slice(&[0; HOLE]);
        want.extend_from_slice(b"src/lib.rs!");
        assert_eq!(composed(&made), Ok(want));
        assert_eq!(made, recipe(&parts).expect("a recipe"), "one recipe of one text");

        let bare = Data::new(1, made.bytes.clone(), Relocs::default());
        assert_eq!(composed(&bare), Ok(made.bytes.clone()), "the control: no relocation, no name");

        let offset: Relocs = (made.relocs.iter())
            .map(|it| Relocation { at: it.at, kind: it.kind, target: it.target.clone(), addend: 1 })
            .collect();
        let planted = Data::new(1, made.bytes.clone(), offset);
        assert!(
            matches!(composed(&planted), Err(TextError::NotAHole { .. })),
            "the control: a hole with an addend composed"
        );
    }
}
