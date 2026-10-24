#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
use std::io::{Read, Write};
pub fn read<R: Read>(mut r: R) -> usize {
    let mut buf = [0u8; 4];
    r.read(&mut buf).unwrap_or(0)
}
pub fn by_ref<R: Read>(mut r: R) -> R { let _ = r.by_ref(); r }
pub fn write<W: Write>(mut w: W) { let _ = w.flush(); }
pub fn dyn_write(mut w: &mut dyn Write) { let _ = w.flush(); }
pub fn sink() -> usize {
    let mut out = Vec::new();
    let _ = out.write(b"x");
    let mut data: &[u8] = b"ab";
    let mut s = String::new();
    let _ = data.read_to_string(&mut s);
    out.len() + s.len()
}
pub fn default_trait<T: Default + Clone>() -> T {
    let mut t = T::default();
    t.clone()
}
