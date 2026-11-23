//! A program's imported symbols: every outside function an `extern "C"` block declares and the
//! program calls, named once ([`Imports::intern`]) and resolved once for the whole program
//! ([`Imports::resolve`]), before any code runs. A symbol the process lacks stops the run
//! with an error naming it ([`Absent`]); nothing is looked up lazily, retried, or bound to a
//! stand-in.
//!
//! The lookup is passed in (sans-IO): the launcher gives `dlsym(RTLD_DEFAULT, name)` (the
//! symbol as written, no leading underscore: `dlsym` adds Mach-O's), a test a table.

use core::alloc::Allocator;
use core::fmt;

/// An imported symbol of the program: its row in [`Imports`], in the order first named.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SymbolId(pub u32);

/// A name that cannot be a symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadName {
    Empty,
    /// A NUL byte: `dlsym` reads a C string.
    Nul,
    /// More than 4 GiB of names.
    TooMany,
}

impl fmt::Display for BadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BadName::Empty => "an imported symbol with an empty name",
            BadName::Nul => "an imported symbol whose name holds a NUL byte",
            BadName::TooMany => "the program's imported symbol names pass 4 GiB",
        })
    }
}

impl core::error::Error for BadName {}

/// Symbols the process does not have: the first by id, and how many others.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Absent<'i> {
    pub name: &'i str,
    pub others: u32,
}

impl fmt::Display for Absent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the program imports `{}`, which no loaded library defines", self.name)?;
        if self.others > 0 {
            write!(f, " ({} more imported symbols are absent)", self.others)?;
        }
        Ok(())
    }
}

impl core::error::Error for Absent<'_> {}

/// The names, each once.
pub struct Imports<A: Allocator> {
    text: Vec<u8, A>,
    /// Each symbol's name in `text`.
    rows: Vec<(u32, u32), A>,
    /// Ids sorted by name, to find a name again.
    sorted: Vec<SymbolId, A>,
}

/// Every symbol's address, by id.
pub struct Resolved<A: Allocator> {
    addrs: Vec<u64, A>,
}

impl<A: Allocator> Resolved<A> {
    pub fn get(&self, id: SymbolId) -> u64 {
        self.addrs[id.0 as usize]
    }
}

impl<A: Allocator + Copy> Imports<A> {
    pub fn new_in(alloc: A) -> Imports<A> {
        Imports { text: Vec::new_in(alloc), rows: Vec::new_in(alloc), sorted: Vec::new_in(alloc) }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn name(&self, id: SymbolId) -> &str {
        let (at, len) = self.rows[id.0 as usize];
        core::str::from_utf8(&self.text[at as usize..(at + len) as usize]).expect("names are interned from `str`s")
    }

    /// `name`'s id, the same for the same name.
    pub fn intern(&mut self, name: &str) -> Result<SymbolId, BadName> {
        if name.is_empty() {
            return Err(BadName::Empty);
        }
        if name.as_bytes().contains(&0) {
            return Err(BadName::Nul);
        }
        match self.sorted.binary_search_by(|&id| self.name(id).cmp(name)) {
            Ok(k) => Ok(self.sorted[k]),
            Err(k) => {
                let at = u32::try_from(self.text.len()).map_err(|_| BadName::TooMany)?;
                let len = u32::try_from(name.len()).map_err(|_| BadName::TooMany)?;
                at.checked_add(len).ok_or(BadName::TooMany)?;
                let id = SymbolId(self.rows.len() as u32);
                self.text.extend_from_slice(name.as_bytes());
                self.rows.push((at, len));
                self.sorted.insert(k, id);
                Ok(id)
            }
        }
    }

    /// Every symbol's address, `lookup` asked once per symbol in id order; the absent named.
    pub fn resolve<B: Allocator>(&self, mut lookup: impl FnMut(&str) -> Option<u64>, alloc: B) -> Result<Resolved<B>, Absent<'_>> {
        let mut addrs = Vec::with_capacity_in(self.rows.len(), alloc);
        let mut absent: Option<Absent<'_>> = None;
        for i in 0..self.rows.len() {
            let name = self.name(SymbolId(i as u32));
            match lookup(name) {
                Some(addr) if addr != 0 => addrs.push(addr),
                _ => match &mut absent {
                    Some(a) => a.others += 1,
                    None => absent = Some(Absent { name, others: 0 }),
                },
            }
        }
        match absent {
            Some(a) => Err(a),
            None => Ok(Resolved { addrs }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;
    use std::cell::Cell;

    use super::{Absent, BadName, Imports, SymbolId};

    #[test]
    fn each_name_is_one_symbol_resolved_once() {
        let mut i = Imports::new_in(Global);
        let write = i.intern("write").unwrap();
        let exit = i.intern("exit").unwrap();
        assert_eq!(i.intern("write"), Ok(write), "a name named twice is one symbol");
        assert_eq!((write, exit, i.len()), (SymbolId(0), SymbolId(1), 2));
        let asked = Cell::new(0);
        let r = i
            .resolve(
                |n| {
                    asked.set(asked.get() + 1);
                    match n {
                        "write" => Some(0x1000),
                        "exit" => Some(0x2000),
                        _ => None,
                    }
                },
                Global,
            )
            .unwrap();
        assert_eq!((r.get(write), r.get(exit), asked.get()), (0x1000, 0x2000, 2), "one lookup per symbol");
        assert_eq!(i.intern(""), Err(BadName::Empty));
        assert_eq!(i.intern("a\0b"), Err(BadName::Nul));
    }

    #[test]
    fn an_absent_symbol_is_refused_by_name() {
        let mut i = Imports::new_in(Global);
        for n in ["write", "blaze_absent_one", "blaze_absent_two"] {
            i.intern(n).unwrap();
        }
        let Err(e) = i.resolve(|n| (n == "write").then_some(8), Global) else { panic!("resolved") };
        assert_eq!(e, Absent { name: "blaze_absent_one", others: 1 });
        assert_eq!(e.to_string(), "the program imports `blaze_absent_one`, which no loaded library defines (1 more imported symbols are absent)");
    }
}
