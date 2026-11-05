//! ARM core register names.
//!
//! There are only sixteen registers, but a great many spellings of them: the
//! APCS names (`a1`-`a4`, `v1`-`v8`), the role names (`sp`, `lr`, `pc`, `fp`,
//! `ip`, `sl`, `sb`) and the plain `r0`-`r15`. All of them encode as a number,
//! so the whole table is a name-to-number map.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A core register, held as its 0-15 encoding.
pub type Reg = u8;

pub const SP: Reg = 13;
pub const LR: Reg = 14;
pub const PC: Reg = 15;

#[rustfmt::skip]
static ALIASES: &[(&str, Reg)] = &[
    ("a1", 0), ("a2", 1), ("a3", 2), ("a4", 3),
    ("v1", 4), ("v2", 5), ("v3", 6), ("v4", 7),
    ("v5", 8), ("v6", 9), ("v7", 10), ("v8", 11),
    ("sb", 9), ("tr", 9), ("sl", 10), ("fp", 11),
    ("ip", 12), ("sp", 13), ("r13", 13),
    ("lr", 14), ("r14", 14), ("pc", 15), ("r15", 15),
];

fn table() -> &'static HashMap<&'static str, Reg> {
    static T: OnceLock<HashMap<&'static str, Reg>> = OnceLock::new();
    T.get_or_init(|| {
        // `r0`-`r15` are spelled out rather than parsed so that a stray `r16`
        // is an unknown name instead of a silently truncated register.
        const NUMBERED: [&str; 16] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "r13",
            "r14", "r15",
        ];
        let mut m = HashMap::new();
        for (i, n) in NUMBERED.iter().enumerate() {
            m.insert(*n, i as Reg);
        }
        for (n, r) in ALIASES {
            m.insert(*n, *r);
        }
        m
    })
}

/// Looks a register up by lowercase name.
pub fn lookup(name: &str) -> Option<Reg> {
    table().get(name).copied()
}

pub fn is_register(name: &str) -> bool {
    table().contains_key(name)
}

/// The canonical spelling, for diagnostics.
pub fn name_of(r: Reg) -> String {
    match r {
        13 => "sp".into(),
        14 => "lr".into(),
        15 => "pc".into(),
        n => format!("r{n}"),
    }
}
