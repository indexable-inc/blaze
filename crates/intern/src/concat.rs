//! Interning a name made of parts (`target_` and a key, `tool::lint`, `f` and a digit string)
//! without first joining them on the heap.

use crate::Symbol;

/// Parts up to this many bytes in all are joined on the stack.
const STACK: usize = 64;

impl Symbol {
    /// [`Symbol::intern`] of `parts` joined, with no heap allocation when they come to at most
    /// 64 bytes.
    pub fn intern_concat(parts: &[&str]) -> Symbol {
        let len = parts.iter().map(|it| it.len()).sum::<usize>();
        let mut stack = [0u8; STACK];
        let mut heap;
        let buf: &mut [u8] = if len <= STACK {
            &mut stack[..len]
        } else {
            // alloc: a name longer than 64 bytes, as the joined copies did
            heap = vec![0u8; len];
            &mut heap
        };
        let mut at = 0;
        for part in parts {
            buf[at..at + part.len()].copy_from_slice(part.as_bytes());
            at += part.len();
        }
        Symbol::intern(std::str::from_utf8(buf).expect("`str`s joined are one"))
    }
}

#[cfg(test)]
mod tests {
    use crate::Symbol;

    /// The copies this replaced: the parts joined into a growable buffer, then interned.
    fn joined_reference(parts: &[&str]) -> Symbol {
        let mut text = Vec::new();
        for part in parts {
            text.extend_from_slice(part.as_bytes());
        }
        Symbol::intern(std::str::from_utf8(&text).expect("two names joined"))
    }

    #[test]
    fn same_symbol_as_joining_first() {
        let long = "x".repeat(70);
        let cases: &[&[&str]] = &[
            &[],
            &[""],
            &["target_", "os"],
            &["target_", ""],
            &["clippy", "::", "needless_return"],
            &["rustdoc", "::", "broken_intra_doc_links"],
            &["f", "0"],
            &["f", "18446744073709551615"],
            &["Ünï", "cödé"],
            &[&long],
            &["target_", &long],
            &[&long[..60], "::", "four"],
        ];
        for parts in cases {
            assert_eq!(Symbol::intern_concat(parts), joined_reference(parts), "{parts:?}");
            assert_eq!(Symbol::intern_concat(parts).as_str(), parts.concat());
        }
    }
}
