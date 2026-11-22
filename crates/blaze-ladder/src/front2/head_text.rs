//! An item's text from its first token of syntax: the outer attributes and comments before it
//! skipped (doc comments are outer attributes: the Reference, "Comments", "Doc comments":
//! "`/// Foo` is turned into `#[doc="Foo"]`").
//!
//! The names library hands an item's whole text, attributes included (blaze-scope task/lower.rs
//! `ItemText::text`, `Member::text`). A scan of the HEAD for a lifetime (`'a`) must not read the
//! docs: alloc/src/rc.rs documents `Rc` as "'Rc' stands for 'Reference Counted'", and a scan of
//! the whole text took `Rc` for an ADT that declares lifetimes, so every `Rc<T>` got an origin
//! and a call returning one was refused `ElisionAmbiguous` (front2 borrows.rs' scan of an ADT's
//! head, gone since D155 counts from the items record; the same for a documented fn's
//! `writes_lifetime`: "it's" read as `'s`).

/// `text` after its leading whitespace, comments (`//..`, `/*..*/` nested) and outer attributes
/// (`#[..]`, brackets balanced, string literals skipped). A text that ends inside one of them
/// is returned empty: there is no head after it.
pub fn after_outer_trivia(text: &str) -> &str {
    let b = text.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i = past_block_comment(b, i);
        } else if b[i..].starts_with(b"#[") {
            i = past_attribute(b, i + 1);
        } else {
            return text.get(i..).unwrap_or("");
        }
    }
}

/// The index after the block comment opening at `at` (nested, as the Reference's "Comments").
fn past_block_comment(b: &[u8], at: usize) -> usize {
    let mut depth = 0u32;
    let mut i = at;
    while i < b.len() {
        if b[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if b[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// The index after the bracketed group opening at `open` (`b[open] == b'['`): brackets of every
/// kind balanced, string literals (`"..."` with escapes, raw `r#"..."#`) skipped whole.
fn past_attribute(b: &[u8], open: usize) -> usize {
    let mut depth = 0u32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'[' | b'(' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b')' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            b'"' => i = past_string(b, i),
            b'r' if b.get(i + 1).is_some_and(|&c| c == b'"' || c == b'#') => i = past_raw_string(b, i),
            _ => i += 1,
        }
    }
    b.len()
}

/// The index after the string literal whose opening quote is at `q`.
fn past_string(b: &[u8], q: usize) -> usize {
    let mut i = q + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The index after the raw string literal starting at `r` (`r"..."`, `r#"..."#`, ..); a bare
/// `r#ident` (a raw identifier) is stepped over as one byte.
fn past_raw_string(b: &[u8], r: usize) -> usize {
    let mut hashes = 0;
    let mut i = r + 1;
    while i < b.len() && b[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    if b.get(i) != Some(&b'"') {
        return r + 1;
    }
    i += 1;
    while i < b.len() {
        if b[i] == b'"' && b.len() >= i + 1 + hashes && b[i + 1..i + 1 + hashes].iter().all(|&c| c == b'#') {
            return i + 1 + hashes;
        }
        i += 1;
    }
    b.len()
}

/// `Rc`'s head as the library writes it: its docs hold apostrophes, its generics no lifetime;
/// after the trivia the head is `pub struct Rc<..>`, with no `'`. A head that does declare one
/// keeps it, and an attribute's string with `]` or `'` inside is skipped whole.
/// Prints "head text: Rc has no lifetime, Ref<'b> has one".
#[test]
fn docs_and_attributes_are_not_the_head() {
    let rc = "/// A single-threaded reference-counting pointer. 'Rc' stands for 'Reference\n/// Counted'.\n/* it's /* nested */ */\n#[doc = \"it's ] here\"]\n#[rustc_diagnostic_item = \"Rc\"]\npub struct Rc<T: ?Sized, A = Global> { ptr: P }";
    let head = after_outer_trivia(rc);
    assert!(head.starts_with("pub struct Rc<"), "{head:?}");
    assert!(!head.split(['{', '(', ';']).next().unwrap_or("").contains('\''), "{head:?}");
    let r = after_outer_trivia("/// docs\n#[must_use]\npub struct Ref<'b, T: ?Sized + 'b> { v: &'b T }");
    assert!(r.starts_with("pub struct Ref<'b"), "{r:?}");
    assert_eq!(after_outer_trivia("/// only docs"), "");
    std::println!("head text: Rc has no lifetime, Ref<'b> has one");
}
