//! String interning.
//!
//! Mnemonics, register names, symbol names and directive names are compared
//! constantly; interning turns all of that into `u32` equality.

use std::collections::HashMap;
use std::fmt;

/// An interned string. Cheap to copy, compare and hash.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(u32);

impl Name {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({})", self.0)
    }
}

#[derive(Default)]
pub struct Interner {
    map: HashMap<Box<str>, Name>,
    strings: Vec<Box<str>>,
}

impl Interner {
    pub fn new() -> Interner {
        Interner::default()
    }

    pub fn intern(&mut self, s: &str) -> Name {
        if let Some(&n) = self.map.get(s) {
            return n;
        }
        let n = Name(self.strings.len() as u32);
        let boxed: Box<str> = s.into();
        self.strings.push(boxed.clone());
        self.map.insert(boxed, n);
        n
    }

    /// Interns the lowercase form of `s`, avoiding an allocation when `s` is
    /// already lowercase (the common case for mnemonics).
    pub fn intern_lower(&mut self, s: &str) -> Name {
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            self.intern(&s.to_ascii_lowercase())
        } else {
            self.intern(s)
        }
    }

    pub fn get(&self, n: Name) -> &str {
        &self.strings[n.0 as usize]
    }

    /// Looks up an existing entry without creating one.
    pub fn lookup(&self, s: &str) -> Option<Name> {
        self.map.get(s).copied()
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_resolves() {
        let mut i = Interner::new();
        let a = i.intern("mov");
        let b = i.intern("mov");
        let c = i.intern("add");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(i.get(a), "mov");
        assert_eq!(i.get(c), "add");
        assert_eq!(i.len(), 2);
    }

    #[test]
    fn intern_lower_folds_case() {
        let mut i = Interner::new();
        assert_eq!(i.intern_lower("MOV"), i.intern_lower("mov"));
        let n = i.intern_lower("MoV");
        assert_eq!(i.get(n), "mov");
    }
}
