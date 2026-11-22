//! Trains a dictionary for one record kind and writes it as a record named by its id.
//!
//!     examplerun.sh blaze-squeeze train RECS MAX_BYTES OUT_DIR
//!
//! `RECS` holds the samples, each a little-endian `u32` length and its bytes (what blaze-scope's
//! pipeline example writes with `--dump`); the dictionary lands at `OUT_DIR/<its id>.dict`, and
//! its id, size and the samples' squeezed size with and without it are printed.

#![feature(allocator_api)]
#![expect(clippy::print_stdout, reason = "a tool prints what it made")]

use std::alloc::Global;
use std::path::PathBuf;

use blaze_squeeze::{Decoder, Dictionary, Encoder};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [recs, max, out] = &args[..] else { panic!("usage: train RECS MAX_BYTES OUT_DIR") };
    let max: usize = max.parse().unwrap_or_else(|e| panic!("MAX_BYTES {max}: {e}"));
    let bytes = std::fs::read(recs).unwrap_or_else(|e| panic!("{recs}: {e}"));
    let mut samples = vec![];
    let mut at = 0;
    while at < bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        samples.push(&bytes[at + 4..at + 4 + len]);
        at += 4 + len;
    }
    let dict = Dictionary::train(&samples, max, Global).unwrap_or_else(|e| panic!("{recs}: {e}"));
    let path = PathBuf::from(out).join(format!("{}.dict", dict.id().0));
    std::fs::create_dir_all(out).unwrap_or_else(|e| panic!("{out}: {e}"));
    std::fs::write(&path, dict.bytes()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (mut plain, mut with) = (Encoder::new::<Global>(3, None).unwrap(), Encoder::new(3, Some(&dict)).unwrap());
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let mut back = Decoder::new().unwrap();
    for s in &samples {
        plain.squeeze(s, &mut a).unwrap();
        let from = b.len();
        with.squeeze(s, &mut b).unwrap();
        let mut out = Vec::new();
        back.unsqueeze(&b[from..], |id| (id == dict.id()).then_some(&dict), s.len(), &mut out).unwrap();
        assert_eq!(&out[..], *s);
    }
    println!(
        "{}: {} bytes, from {} samples of {} bytes; zstd 3 alone {} bytes, with it {} (the samples it was trained on)",
        path.display(),
        dict.bytes().len(),
        samples.len(),
        bytes.len() - 4 * samples.len(),
        a.len(),
        b.len()
    );
}
