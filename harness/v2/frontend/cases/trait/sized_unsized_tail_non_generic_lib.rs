//@ expect: accept
//@ edition: 2021
// Non-generic structs with an unsized last field (`str`, `[u64]`), used behind references.
#![crate_type = "lib"]

pub struct Name {
    pub len: u8,
    pub bytes: str,
}

pub struct Ints {
    pub head: u32,
    pub rest: [u64],
}

pub fn name_len(n: &Name) -> usize {
    n.bytes.len() + n.len as usize
}

pub fn ints_sum(i: &Ints) -> u64 {
    i.head as u64 + i.rest.iter().sum::<u64>()
}
