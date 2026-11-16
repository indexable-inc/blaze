//! Git's protocol version 2 over smart HTTP (`gitprotocol-v2`): the capability advertisement,
//! `ls-refs` and `fetch` with `deepen`, each a request body to POST and a parser of its answer.
//! Bytes in, bytes out: the driver ([`crate::fetch`]) moves them.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::{
    oid::Oid,
    pkt::{self, Lines, Pkt, PktWhy},
};

/// The client's name, sent as `agent=`.
pub const AGENT: &[u8] = b"agent=blaze\n";

/// What the server's v2 advertisement must offer: `ls-refs`, and `fetch` with `shallow`.
pub fn check_advertisement(body: &[u8]) -> Result<(), PktWhy> {
    let mut lines = Lines::new(body);
    // Smart HTTP may open with `# service=git-upload-pack` and a flush (git-http-backend does).
    let mut first = lines.expect_line()?;
    if let Pkt::Data(line) = first
        && pkt::text(line) == b"# service=git-upload-pack"
    {
        if lines.expect_line()? != Pkt::Flush {
            return Err(PktWhy::Unexpected);
        }
        first = lines.expect_line()?;
    }
    if first != Pkt::Data(b"version 2\n") {
        return Err(PktWhy::Unexpected);
    }
    let (mut ls_refs, mut shallow) = (false, false);
    loop {
        match lines.expect_line()? {
            Pkt::Flush => break,
            Pkt::Data(cap) => {
                let cap = pkt::text(cap);
                if cap == b"ls-refs" || cap.starts_with(b"ls-refs=") {
                    ls_refs = true;
                }
                if let Some(features) = cap.strip_prefix(b"fetch=") {
                    shallow |= features.split(|&b| b == b' ').any(|f| f == b"shallow");
                }
            }
            Pkt::Delim | Pkt::End => return Err(PktWhy::Unexpected),
        }
    }
    match ls_refs && shallow {
        true => Ok(()),
        false => Err(PktWhy::Unexpected),
    }
}

/// The `ls-refs` request for refs whose names start with one of `prefixes`, peeled.
pub fn ls_refs<B: Allocator>(prefixes: &[&str], out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    pkt::data(b"command=ls-refs\n", out)?;
    pkt::data(AGENT, out)?;
    pkt::data(b"object-format=sha1\n", out)?;
    pkt::delim(out)?;
    pkt::data(b"peel\n", out)?;
    pkt::data(b"symrefs\n", out)?;
    for prefix in prefixes {
        pkt::line(&[b"ref-prefix ", prefix.as_bytes(), b"\n"], out)?;
    }
    pkt::flush(out)
}

/// The commit a ref names, from an `ls-refs` answer: the peeled id for an annotated tag.
pub fn find_ref(body: &[u8], name: &str) -> Result<Option<Oid>, PktWhy> {
    let mut lines = Lines::new(body);
    loop {
        match lines.expect_line()? {
            Pkt::Flush => return Ok(None),
            Pkt::Data(line) => {
                let line = pkt::text(line);
                let mut parts = line.split(|&b| b == b' ');
                let oid = parts.next().and_then(Oid::parse_hex).ok_or(PktWhy::Unexpected)?;
                let refname = parts.next().ok_or(PktWhy::Unexpected)?;
                if refname != name.as_bytes() {
                    continue;
                }
                let peeled = parts.find_map(|attr| attr.strip_prefix(b"peeled:"));
                return match peeled {
                    Some(hex) => Oid::parse_hex(hex).map(Some).ok_or(PktWhy::Unexpected),
                    None => Ok(Some(oid)),
                };
            }
            Pkt::Delim | Pkt::End => return Err(PktWhy::Unexpected),
        }
    }
}

/// The `fetch` request for `want` and its history to `depth` commits, all in one round
/// (`done`), so the answer is the pack and the shallow list, no negotiation.
pub fn fetch<B: Allocator>(want: Oid, depth: u32, out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    pkt::data(b"command=fetch\n", out)?;
    pkt::data(AGENT, out)?;
    pkt::data(b"object-format=sha1\n", out)?;
    pkt::delim(out)?;
    pkt::line(&[b"want ", &want.hex(), b"\n"], out)?;
    let mut digits = [0u8; 10];
    pkt::line(&[b"deepen ", decimal(depth, &mut digits), b"\n"], out)?;
    pkt::data(b"ofs-delta\n", out)?;
    pkt::data(b"no-progress\n", out)?;
    pkt::data(b"done\n", out)?;
    pkt::flush(out)
}

/// The pack in a `fetch` answer (its `shallow-info` and other sections passed over), at most
/// `most` bytes, appended to `out`.
pub fn fetched_pack<B: Allocator>(body: &[u8], most: u64, out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    let mut lines = Lines::new(body);
    loop {
        match lines.expect_line()? {
            Pkt::Data(b"packfile\n") => return pkt::sideband(&mut lines, most, out),
            // Another section (shallow-info, wanted-refs): its lines up to a delimiter.
            Pkt::Data(_) => loop {
                match lines.expect_line()? {
                    Pkt::Delim => break,
                    Pkt::Data(_) => {}
                    Pkt::Flush | Pkt::End => return Err(PktWhy::Unexpected),
                }
            },
            Pkt::Flush | Pkt::Delim | Pkt::End => return Err(PktWhy::Unexpected),
        }
    }
}

fn decimal(mut value: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buf[at..];
        }
    }
}
