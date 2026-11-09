//! The explorer core a producer serves (docs/v2/plan/design/explore-core.md, decision 3): the
//! page's state and views compiled to `wasm32-unknown-unknown`, served by the program whose feed
//! it folds, so the page always runs the core that program was released with. `hello` names it
//! (`core: {abi, sha256, bytes}`) and the `core` op answers its bytes; the page fetches it once
//! per sha256 and caches it.
//!
//! The module says its ABI in a custom section, [`SECTION`], four bytes little-endian (written
//! by blaze-explore-core's `abi` module), so a server reads it without running the module.

/// The custom section holding a core's ABI version, `u32` little-endian.
pub const SECTION: &str = "ix.explore.abi";

/// The most bytes a core may be: 64 MiB. A core is a few hundred KiB; past this is no core.
pub const MOST_BYTES: usize = 64 << 20;

/// A core as served: its ABI, the hex SHA-256 of its bytes, and the bytes.
#[derive(Clone, Debug)]
pub struct CoreWasm {
    pub abi: u32,
    pub sha256: String,
    pub bytes: std::sync::Arc<[u8]>,
}

impl CoreWasm {
    /// The core `bytes` are, read from `from` (named in the errors): refused when they are no
    /// wasm module, are over [`MOST_BYTES`], or have no [`SECTION`] of four bytes.
    pub fn new(bytes: Vec<u8>, from: &str) -> Result<CoreWasm, String> {
        if bytes.len() > MOST_BYTES {
            return Err(format!(
                "the explorer core {from} is {} bytes, over the {MOST_BYTES} a core may be",
                bytes.len()
            ));
        }
        let abi = abi(&bytes).map_err(|why| format!("the explorer core {from}: {why}"))?;
        let sha256 = hex(&blaze_sha256::digest(&bytes));
        Ok(CoreWasm { abi, sha256, bytes: bytes.into() })
    }
}

/// `digest` as lowercase hex.
fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    // alloc: once a core
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The ABI in module `bytes`' [`SECTION`]. The walk takes one section a step, each step past
/// the last, so it ends within the module's length.
fn abi(bytes: &[u8]) -> Result<u32, String> {
    const MAGIC: [u8; 8] = [0, b'a', b's', b'm', 1, 0, 0, 0];
    let Some(mut rest) = bytes.strip_prefix(&MAGIC) else {
        return Err("it is not a wasm module of version 1 (no `\\0asm` and 1 at its start)".to_owned());
    };
    while let Some((&id, after)) = rest.split_first() {
        let (size, after) = leb128(after).ok_or("a section's size is cut short")?;
        let size = usize::try_from(size).map_err(|_| "a section's size is past memory")?;
        let (section, next) = after
            .split_at_checked(size)
            .ok_or_else(|| format!("a section of {size} bytes runs past the module's end"))?;
        if id == 0 {
            let (len, named) = leb128(section).ok_or("a custom section's name is cut short")?;
            let len = usize::try_from(len).map_err(|_| "a custom section's name is past memory")?;
            let (name, content) = named
                .split_at_checked(len)
                .ok_or("a custom section's name runs past the section")?;
            if name == SECTION.as_bytes() {
                let abi: [u8; 4] = content.try_into().map_err(|_| {
                    format!("its `{SECTION}` section is {} bytes, not 4", content.len())
                })?;
                return Ok(u32::from_le_bytes(abi));
            }
        }
        rest = next;
    }
    Err(format!(
        "it has no `{SECTION}` section saying its ABI: it is no explorer core, or one built before cores said it"
    ))
}

/// An unsigned LEB128 of at most 5 bytes (a `u32`), and what follows it.
fn leb128(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for (at, &byte) in bytes.iter().enumerate().take(5) {
        value |= u64::from(byte & 0x7f) << (7 * at);
        if byte & 0x80 == 0 {
            return Some((value, &bytes[at + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0, b'a', b's', b'm', 1, 0, 0, 0];
        for (id, body) in sections {
            out.push(*id);
            out.push(u8::try_from(body.len()).unwrap());
            out.extend_from_slice(body);
        }
        out
    }

    fn custom(name: &str, content: &[u8]) -> (u8, Vec<u8>) {
        let mut body = vec![u8::try_from(name.len()).unwrap()];
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(content);
        (0, body)
    }

    #[test]
    fn the_abi_is_read_from_its_section_past_others() {
        let bytes = module(&[(1, vec![0]), custom("name", b"x"), custom(SECTION, &7u32.to_le_bytes())]);
        let core = CoreWasm::new(bytes.clone(), "test.wasm").unwrap();
        assert_eq!(core.abi, 7);
        assert_eq!(core.sha256, hex(&blaze_sha256::digest(&bytes)));
        assert_eq!(core.sha256.len(), 64);
        assert_eq!(&*core.bytes, &bytes[..]);
    }

    #[test]
    fn a_module_without_the_section_or_cut_short_is_refused_naming_it() {
        let none = CoreWasm::new(module(&[custom("name", b"x")]), "a.wasm").unwrap_err();
        assert!(none.contains("a.wasm") && none.contains(SECTION), "{none}");
        let mut cut = module(&[custom(SECTION, &1u32.to_le_bytes())]);
        cut.truncate(cut.len() - 2);
        assert!(CoreWasm::new(cut, "b.wasm").unwrap_err().contains("past the module's end"));
        assert!(CoreWasm::new(b"not wasm".to_vec(), "c.wasm").unwrap_err().contains("not a wasm"));
    }
}
