//! Outlines: a parse that leaves every `fn` body unparsed.
//!
//! In an outline ([`crate::TopEntryPoint::parse_outline`]) a `fn` item's body block is skipped
//! as the balanced `{ ... }` of its tokens and emitted as one lazy node
//! ([`crate::OutlineStep::Lazy`]) of kind `BLOCK_EXPR`. Parsing the block's text alone with
//! [`crate::Reparser::for_node`] of `BLOCK_EXPR` then gives exactly the subtree a full parse
//! makes there, because the grammar takes each `{` and `}` in a block as the delimiters they
//! match, as rustc's token trees do. A few error recoveries take a brace as a plain token
//! instead, all of them in code rustc rejects; [`lazy_block_len`] refuses a body holding one
//! it can see from the tokens, and that `fn` is parsed in full in the outline, which makes the
//! same tree. The one it cannot see, a brace where a generic parameter list expects a
//! parameter (`fn f<T, {>()`) in a `fn` nested in a body, makes the body's part fail its check
//! when parsed; outlines are for the files of dependency crates, which rustc builds.

use smallvec::SmallVec;

use crate::{
    SyntaxKind::{self, EOF, IDENT, OUTER_DOC_COMMENT},
    T, Tokens,
};

/// The kinds [`lazy_block_len`]'s `match` has an arm for. Every other token, most of a body,
/// only moves the scan on, unless it is the one right after an attribute.
const WATCHED: [bool; SyntaxKind::__LAST as usize + 1] = {
    let mut watched = [false; SyntaxKind::__LAST as usize + 1];
    let kinds = [EOF, T!['{'], T!['}'], T!['['], T![']'], T![#], T![<], OUTER_DOC_COMMENT, IDENT];
    let mut i = 0;
    while i < kinds.len() {
        watched[kinds[i] as usize] = true;
        i += 1;
    }
    watched
};

/// The number of tokens of the balanced `{ ... }` starting at token `start` of `input`, when an
/// outline may leave it unparsed: its braces balance before the input ends, and it holds none
/// of the tokens whose error recovery takes a brace as a plain token: an outer attribute or a
/// doc comment right before a `}` (the statement expected after it takes the `}`), a `#` or a
/// `<` right before a brace (the expression or generic parameter expected there takes it) and
/// `builtin #` (whose `asm` operands' recovery takes any token).
pub fn lazy_block_len<'t>(input: impl Into<Tokens<'t>>, start: usize) -> Option<u32> {
    let input = input.into();
    if input.kind(start) != T!['{'] {
        return None;
    }
    let mut depth = 0usize;
    let mut brackets = 0usize;
    // The bracket depths at which the outer attributes being read started.
    let mut attributes: SmallVec<[usize; 4]> = SmallVec::new();
    let mut after_attribute = false;
    let mut at = start;
    loop {
        let kind = input.kind(at);
        if !after_attribute && !WATCHED[kind as usize] {
            at += 1;
            continue;
        }
        let next = input.kind(at + 1);
        let attribute_before = std::mem::replace(&mut after_attribute, false);
        match kind {
            EOF => return None,
            T!['{'] => depth += 1,
            T!['}'] => {
                if attribute_before {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return u32::try_from(at + 1 - start).ok();
                }
            }
            T!['['] => brackets += 1,
            T![']'] => {
                brackets = brackets.saturating_sub(1);
                if attributes.last() == Some(&brackets) {
                    attributes.pop();
                    after_attribute = true;
                }
            }
            T![#] if next == T!['['] => attributes.push(brackets),
            T![#] | T![<] if matches!(next, T!['{'] | T!['}']) => return None,
            OUTER_DOC_COMMENT => after_attribute = true,
            IDENT if input.contextual_kind(at) == T![builtin] && next == T![#] => return None,
            _ => (),
        }
        at += 1;
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;
    use crate::{Edition, Input};

    /// [`lazy_block_len`] as it was before [`WATCHED`]: every token through the `match`.
    fn reference(input: &Input, start: usize) -> Option<u32> {
        let input = Tokens::from(input);
        if input.kind(start) != T!['{'] {
            return None;
        }
        let (mut depth, mut brackets) = (0usize, 0usize);
        let mut attributes: Vec<usize> = Vec::new();
        let mut after_attribute = false;
        let mut at = start;
        loop {
            let (kind, next) = (input.kind(at), input.kind(at + 1));
            let attribute_before = std::mem::replace(&mut after_attribute, false);
            match kind {
                EOF => return None,
                T!['{'] => depth += 1,
                T!['}'] => {
                    if attribute_before {
                        return None;
                    }
                    depth -= 1;
                    if depth == 0 {
                        return u32::try_from(at + 1 - start).ok();
                    }
                }
                T!['['] => brackets += 1,
                T![']'] => {
                    brackets = brackets.saturating_sub(1);
                    if attributes.last() == Some(&brackets) {
                        attributes.pop();
                        after_attribute = true;
                    }
                }
                T![#] if next == T!['['] => attributes.push(brackets),
                T![#] | T![<] if matches!(next, T!['{'] | T!['}']) => return None,
                OUTER_DOC_COMMENT => after_attribute = true,
                IDENT if input.contextual_kind(at) == T![builtin] && next == T![#] => return None,
                _ => (),
            }
            at += 1;
        }
    }

    /// Skipping the tokens no arm watches changes nothing: random token sequences heavy in
    /// braces, brackets, attributes, doc comments, `<`, `builtin #` and tokens of no interest,
    /// from every start.
    #[test]
    fn skipping_unwatched_tokens_keeps_every_answer() {
        let mut rng = SplitMix64::new(17);
        let kinds = [
            T!['{'], T!['}'], T!['['], T![']'], T![#], T![<], OUTER_DOC_COMMENT, IDENT, T![;], T![fn],
            SyntaxKind::INT_NUMBER, T![,], T![>],
        ];
        for _ in 0..4000 {
            let mut input = Input::with_capacity(40);
            for _ in 0..rng.index(40) {
                match kinds[rng.index(kinds.len())] {
                    IDENT if rng.index(4) == 0 => input.push_ident(T![builtin], Edition::CURRENT),
                    IDENT => input.push_ident(IDENT, Edition::CURRENT),
                    kind => input.push(kind, Edition::CURRENT),
                }
            }
            for start in 0..input.len() {
                assert_eq!(lazy_block_len(&input, start), reference(&input, start), "from {start}");
            }
        }
    }
}
