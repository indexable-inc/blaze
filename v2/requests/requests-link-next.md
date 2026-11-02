# Requests to `requests-link-next` (holds `crates/blaze-link/`)

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-link` (child of `dedup-cursor`, lane `dedup`), 2026-10-29. Report:
`/tmp/blaze-lanes/v2-dedup-cursor-link.md`. The cursor: `crates/blaze-format/src/cursor.rs`
(`Reader<'a>`, `Error { at, wanted }`, `Error::as_str`, `impl From<Error> for String`, the `Put`
trait on `Vec<u8>`); its API table is `docs/v2/plan/lanes/dedup/cursor.md`.

Every hand-written little-endian byte cursor and writer helper in blaze-link moves onto it, and
the copies are deleted. `crates/blaze-link/Cargo.toml` already has `blaze-format.workspace =
true`: no manifest change. These build on the LEB128 dedup already applied here
(`v2/requests/mem-backend-link.md` "dedup: LEB128 and RNGs", `v2/requests/requests-link-fuzz.md`
"dedup: `macho/dylib.rs` LEB128"): the cursor's `uleb`/`sleb`/`uleb32`/`sleb32`/`skip_leb` call
the same `blaze_format::leb` functions those requests moved the copies onto, so the LEB rules
(strict past 64 bits; wasm's 5 bytes and 32 bits) are unchanged. C3 to C5 are also the first step
of `requests-link-fuzz.md` A8 (one `.eh_frame` decoder for both writers): A8 then moves `fdes`
and `walk`, which already call the one decoder.

Functions are named, not lines: `requests-link-next-writers-elf` and
`-writers-unwind` are editing `elf/dynamic.rs` and `macho/unwind.rs` now. Apply C4, C5 and C9
after their items land. Each item stands alone except C4 and C5, which need C3.

Where the error words change, no test checks them (checked: every `contains`/`unwrap_err` in
the crate); the cursor's `Display` (`at byte 12: 4 bytes wanted, 2 left`) or `as_str` replaces
"cut short", "truncated", "a string without its end". Error words a test does check stay.

A missed rename in C6 would compile and misread (the cursor's `u32()` is 4 fixed bytes, wasm's
was a LEB128), so C6 lists every line; every other rename fails to build if missed.

> forwarded to requests-link-next (holds `crates/blaze-link/`) (requests, pass 20; told by tmux).
> applied by requests (child requests-link-next): C1-C9 applied (children requests-link-next-cursor-eh: C3-C5, requests-link-next-cursor-rest: C1, C2, C6-C9), each answered under its heading; C8 keeps the trie's error words, which `malformed_tries` checks.

### C1. `position.rs`: `Reader` (the span table's)

Add `use blaze_format::cursor::{Put, Reader};`. Delete `struct Reader` and its `impl` (`take`,
`word`).

`SpanTable::encode`, from `out.extend_from_slice(&MAGIC);` to the end of the span loop, after:

```rust
        out.put_bytes(&MAGIC);
        out.put_u32(self.files.len() as u32);
        for file in &self.files {
            out.put_bytes_u32(file.as_bytes());
        }
        out.put_u32(self.spans.len() as u32);
        for span in &self.spans {
            for word in [span.file, span.line, span.column] {
                out.put_u32(word);
            }
        }
```

(`put_bytes_u32` panics on a name past 4 GiB, where the copy's `as u32` wrote a wrong length.)

`Spans::new`, after:

```rust
    pub fn new(bytes: &[u8]) -> Result<Spans, String> {
        let mut read = Reader::new(bytes);
        if read.take(8)? != MAGIC {
            // alloc: error path
            return Err("not a span table: its magic is wrong".to_owned());
        }
        // Each file takes at least its length's 4 bytes, so the index is no longer than the table.
        let files = read.count_u32(4)?;
        let index = files * 4;
        // An exact-size iterator collects into the `Arc` itself.
        // alloc: the table's one buffer, once per table
        let mut buf: Arc<[u8]> = std::iter::repeat_n(0, index + bytes.len()).collect();
        let out = Arc::get_mut(&mut buf).expect("just made");
        for file in 0..files {
            let start = read.at() + 4;
            read.str_u32()?;
            out[file * 4..file * 4 + 4].copy_from_slice(&((index + start) as u32).to_le_bytes());
        }
        let spans = read.count_u32(SPAN)?;
        let spans_at = read.at();
        let rest = read.take(spans * SPAN)?;
        if !read.is_done() {
            // alloc: error path
            return Err("bytes after a span table".to_owned());
        }
        for span in rest.chunks_exact(SPAN) {
            let file = u32::from_le_bytes(span[..4].try_into().expect("four bytes"));
            if file as usize >= files {
                // alloc: error path
                return Err(format!("a span in file {file} of {files}"));
            }
        }
        out[index..].copy_from_slice(bytes);
        let spans_at = u32::try_from(index + spans_at).map_err(|_| "a span table past 4 GiB")?;
        Ok(Spans { buf, files: files as u32, spans_at, spans: spans as u32 })
    }
```

Tests unchanged (`a_bad_table_is_refused_not_misread` checks `is_err` only).

> applied by requests (child requests-link-next): as written (`Reader` gone; `encode` on `Put`, `Spans::new` on the cursor with `count_u32`/`str_u32`).

### C2. `macho/input.rs`: `Cursor`, `linker_option`, the test writers

This file has its own `struct Reader` (the object reader), so the cursor is `cursor::Reader`.
Add `use blaze_format::cursor;` to the module and `use blaze_format::cursor::Put;` to
`mod tests`.
Replace `const DW_EH_PE_OMIT: u8 = 0xff;` with `use crate::unwind::DW_EH_PE_OMIT;` (made in C3).
Delete `struct Cursor` and its `impl` (`take`, `u8`, `u32`, `uleb`, `string`, `pointer`).

`linker_option`, after:

```rust
fn linker_option(name: &str, command: &[u8]) -> Result<String, String> {
    let mut words = cursor::Reader::new(command);
    let count = words
        .skip(8)
        .and_then(|()| words.u32())
        // alloc: error path
        .map_err(|_| format!("{name}: a truncated LC_LINKER_OPTION"))?;
    let mut option = String::with_capacity(words.left());
    for i in 0..count {
        let word = words
            .cstr()
            // alloc: error path
            .map_err(|_| format!("{name}: an LC_LINKER_OPTION with fewer than {count} strings"))?;
        let word = std::str::from_utf8(word)
            // alloc: error path
            .map_err(|_| format!("{name}: an LC_LINKER_OPTION that isn't UTF-8"))?;
        if i != 0 {
            option.push(' ');
        }
        option.push_str(word);
    }
    Ok(option)
}
```

`Reader::eh_frame`, the record loop, after (positions stay section-absolute: the readers span
`bytes[..end]` and start at `start`):

```rust
        let mut at = 0;
        while at < bytes.len() {
            let length = cursor::Reader::starting_at(bytes, at)
                .and_then(|mut entry| entry.u32())
                .map_err(|it| self.frame_error(s, at, it.as_str()))?;
            if length == 0 {
                break;
            }
            if length == u32::MAX {
                return Err(self.frame_error(s, at, "a 64-bit call frame entry"));
            }
            let start = at + 4;
            let end = start
                .checked_add(length as usize)
                .filter(|&it| it <= bytes.len())
                .ok_or_else(|| self.frame_error(s, at, "a call frame entry longer than the section"))?;
            let mut c = cursor::Reader::starting_at(&bytes[..end], start)
                .map_err(|it| self.frame_error(s, start, it.as_str()))?;
            let id = c.u32().map_err(|it| self.frame_error(s, start, it.as_str()))?;
            if id == 0 {
                personalities.clear();
                let cie = Cie::parse(&mut c, &mut personalities).map_err(|it| self.frame_error(s, start, &it))?;
                cies.push((at, cie));
                for &(field, encoding) in &personalities {
                    self.eh_pointer(s, field, encoding, &covered, &mut by_addr)?;
                }
            } else {
                // The CIE pointer is the distance back from itself to the CIE's length field.
                let cie = start
                    .checked_sub(id as usize)
                    .and_then(|it| cies.binary_search_by_key(&it, |(cie, _)| *cie).ok())
                    .map(|found| cies[found].1)
                    .ok_or_else(|| self.frame_error(s, start, "an FDE whose CIE isn't before it"))?;
                let pc_begin = c.at();
                skip_pointer(&mut c, cie.fde).map_err(|it| self.frame_error(s, c.at(), &it))?;
                skip_pointer(&mut c, cie.fde & 0x0f).map_err(|it| self.frame_error(s, c.at(), &it))?;
                let mut lsda = None;
                if cie.z {
                    c.skip_leb().map_err(|it| self.frame_error(s, c.at(), it.as_str()))?;
                    lsda = cie.lsda.map(|encoding| (c.at(), encoding));
                }
                self.eh_pointer(s, pc_begin, cie.fde, &covered, &mut by_addr)?;
                if let Some((field, encoding)) = lsda {
                    self.eh_pointer(s, field, encoding, &covered, &mut by_addr)?;
                }
            }
            at = end;
        }
```

`Cie::parse`: the parameter becomes `c: &mut cursor::Reader<'_>`; in its body
`c.string()?` -> `c.cstr().map_err(|_| "an unterminated CIE augmentation")?`, each `c.uleb()?`
-> `c.skip_leb()?`, `personalities.push((c.at, encoding));` -> `personalities.push((c.at(),
encoding));`, `c.pointer(encoding)?;` -> `skip_pointer(c, encoding)?;`. `c.u8()?` stays (the
cursor's error becomes the `String` by `From`).

New, where `Cursor` was:

```rust
/// Moves past a pointer of `encoding`'s format.
fn skip_pointer(c: &mut cursor::Reader<'_>, encoding: u8) -> Result<(), String> {
    if encoding == DW_EH_PE_OMIT {
        return Ok(());
    }
    match encoding & 0x0f {
        0x00 | 0x04 | 0x0c => c.skip(8)?,
        0x02 | 0x0a => c.skip(2)?,
        0x03 | 0x0b => c.skip(4)?,
        0x01 | 0x09 => c.skip_leb()?,
        // alloc: error path
        other => return Err(format!("pointer format {other:#x}")),
    }
    Ok(())
}
```

Tests: delete `fn w16`, `fn w32`, `fn w64`; add `use blaze_format::cursor::Put;` to `mod tests`;
every `w16(&mut x, v)` -> `x.put_u16(v)`, `w32(&mut x, v)` -> `x.put_u32(v)`, `w64(&mut x, v)`
-> `x.put_u64(v)`, and where the first argument is already a `&mut Vec<u8>` (`w32(eh, 16)` in
the FDE-building closures) `eh.put_u32(16)`. A missed call fails to build.

> applied by requests (child requests-link-next): as written (`Cursor` gone, `skip_pointer`, `linker_option` on `cursor::Reader`, test `w16`/`w32`/`w64` -> `put_u*`). `DW_EH_PE_OMIT` imported from `crate::unwind`, which requests-link-next-cursor-eh made `pub(crate)` on request.

### C3. core `unwind.rs`: one `.eh_frame` pointer decoder, `split` and the writers on the cursor

Add `use blaze_format::cursor::{self, Put, Reader};`.

New (moved from `elf/dynamic.rs`'s `cie_encoding`, `pointer_encoding`, `Cursor::value`,
`Cursor::pointer`, which `macho/unwind.rs` has a copy of as `cie_encoding`, `valid`,
`Reader::value`, `Reader::pointer`; C4 and C5 delete both):

```rust
pub(crate) const DW_EH_PE_OMIT: u8 = 0xff;
pub(crate) const DW_EH_PE_PCREL: u8 = 0x10;

/// The FDE pointer encoding of the CIE `reader` is in (past its ID), what leads to it checked
/// as gimli checks it. `reader`'s positions are from the start of the section at `section`.
pub(crate) fn cie_encoding(reader: &mut Reader<'_>, section: u64) -> Result<u8, &'static str> {
    const UNKNOWN: &str = "a CIE of an unknown augmentation";
    let version = reader.u8().map_err(cursor::Error::as_str)?;
    if !matches!(version, 1 | 3 | 4) {
        return Err("a CIE of an unknown version");
    }
    let augmentation = reader.cstr().map_err(cursor::Error::as_str)?;
    reader.uleb().map_err(cursor::Error::as_str)?;
    reader.sleb().map_err(cursor::Error::as_str)?;
    if version == 1 {
        reader.u8().map_err(cursor::Error::as_str)?;
    } else {
        reader.uleb().map_err(cursor::Error::as_str)?;
    }
    let mut encoding = 0;
    let mut data: Option<Reader<'_>> = None;
    for (index, &ch) in augmentation.iter().enumerate() {
        match ch {
            b'z' if index == 0 => {
                let len = reader.uleb().map_err(cursor::Error::as_str)?;
                let start = reader.at();
                usize::try_from(len)
                    .ok()
                    .and_then(|it| reader.skip(it).ok())
                    .ok_or("augmentation data longer than its CIE")?;
                let bytes = &reader.bytes()[..reader.at()];
                data = Some(Reader::starting_at(bytes, start).map_err(cursor::Error::as_str)?);
            }
            b'L' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'P' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                let personality = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
                eh_pointer(data, personality, section)?;
            }
            b'R' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                encoding = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'S' => {}
            _ => return Err(UNKNOWN),
        }
    }
    Ok(encoding)
}

/// `encoding` if it is a `DW_EH_PE_*` pointer encoding.
pub(crate) fn pointer_encoding(encoding: u8) -> Result<u8, &'static str> {
    let format = matches!(encoding & 0x0f, 0x00..=0x04 | 0x09..=0x0c);
    if encoding == DW_EH_PE_OMIT || (format && encoding & 0x70 <= 0x50) {
        Ok(encoding)
    } else {
        Err("an unknown pointer encoding")
    }
}

/// A value in `encoding`'s format, signed ones sign-extended.
pub(crate) fn eh_value(reader: &mut Reader<'_>, encoding: u8) -> Result<u64, &'static str> {
    let value = match encoding & 0x0f {
        0x00 | 0x04 | 0x0c => reader.u64(),
        0x01 => reader.uleb(),
        0x02 => reader.u16().map(u64::from),
        0x03 => reader.u32().map(u64::from),
        0x09 => reader.sleb().map(|it| it as u64),
        0x0a => reader.i16().map(|it| it as u64),
        0x0b => reader.i32().map(|it| it as u64),
        _ => return Err("an unknown pointer encoding"),
    };
    value.map_err(cursor::Error::as_str)
}

/// A pointer in `encoding` with only the section's address (`section`) known: absolute or
/// relative to where it is.
pub(crate) fn eh_pointer(reader: &mut Reader<'_>, encoding: u8, section: u64) -> Result<u64, &'static str> {
    if encoding == DW_EH_PE_OMIT {
        return Err("a pointer whose encoding says it is omitted");
    }
    let base = match encoding & 0x70 {
        0x00 => 0,
        DW_EH_PE_PCREL => section.wrapping_add(reader.at() as u64),
        _ => return Err("a pointer relative to a base other than where it is"),
    };
    Ok(base.wrapping_add(eh_value(reader, encoding)?))
}
```

`split`, the loop, after (the `word` closure goes):

```rust
    let mut read = Reader::new(section);
    while read.left() >= 4 {
        let at = read.at();
        let len = read.u32()? as usize;
        if len == 0 {
            break;
        }
        if len == u32::MAX as usize {
            // alloc: error path
            return Err("an .eh_frame record with a 64-bit length".to_owned());
        }
        let Some(body) = read.take(len).ok().filter(|_| len >= 4) else {
            // alloc: error path
            return Err(format!("an .eh_frame record at {at:#x} past the section's end"));
        };
        let end = read.at();
        let id = Reader::new(body).u32()? as usize;
        let cie = match id {
```

and the rest of the loop body as it is, without the final `at = end;`.

`fde_into`: `bytes.extend_from_slice(&range.to_le_bytes());` -> `bytes.put_u32(range);`.
`finish`: `bytes[..4].copy_from_slice(&len.to_le_bytes());` -> `bytes.patch_u32(0, len);`.
The `split` tests check `is_err` and "no CIE", both kept.

> applied by requests (child requests-link-next): `unwind.rs` holds the one decoder (`cie_encoding`, `pointer_encoding`, `eh_value`, `eh_pointer` on `cursor::Reader`); `split` reads with `Reader`; `fde_into` and `finish` use `put_u32` and `patch_u32`. Differs: `DW_EH_PE_OMIT`, `DW_EH_PE_PCREL` and `pointer_encoding` are private (no caller outside). Same bytes, same accepted inputs; "no CIE" and the split errors kept.

### C4. `elf/dynamic.rs`: `Cursor` and the writer trait `Put` go (needs C3)

- Delete `pub(crate) trait Put` and `impl Put for Vec<u8>` (`u8`, `u16`, `u32`, `u64`,
  `align`; `align` has no caller). Add `use blaze_format::cursor::{self, Put, Reader};` and
  `use crate::unwind::{cie_encoding, eh_pointer, eh_value};`; remove
  `use blaze_format::leb::LebError;`.
- Every writer call on a `Vec<u8>` (one argument: `out.u8(v)`, `.u16(v)`, `.u32(v)`, `.u64(v)`)
  becomes `put_u8`/`put_u16`/`put_u32`/`put_u64`. Reader calls (no argument) stay. A missed
  writer call fails to build.
- Delete `const DW_EH_PE_OMIT`, `const DW_EH_PE_PCREL`, `fn cie_encoding`,
  `fn pointer_encoding`, `struct Cursor` and its `impl`.
- `walk`: `let mut reader = Cursor { bytes: &bytes[..next], at: body };` ->
  `let mut reader = Reader::starting_at(&bytes[..next], body).map_err(|it| bad(record, it.as_str()))?;`;
  `reader.u32().map_err(|it| bad(record, it))?` -> `reader.u32().map_err(|it| bad(record, it.as_str()))?`;
  `reader.pointer(encoding, section)` -> `eh_pointer(&mut reader, encoding, section)`;
  `reader.value(encoding)` -> `eh_value(&mut reader, encoding)`.
- `entry`, after:

```rust
fn entry(bytes: &[u8], at: usize) -> Result<Option<(usize, usize)>, &'static str> {
    let mut reader = Reader::starting_at(bytes, at).map_err(cursor::Error::as_str)?;
    let length = match reader.u32().map_err(cursor::Error::as_str)? {
        0 => return Ok(None),
        0xffff_fff0..=0xffff_fffe => return Err("a reserved initial length"),
        0xffff_ffff => reader.u64().map_err(cursor::Error::as_str)?,
        length => u64::from(length),
    };
    let body = reader.at();
    let end = usize::try_from(length)
        .ok()
        .and_then(|it| body.checked_add(it))
        .filter(|it| *it <= bytes.len())
        .ok_or("longer than the section")?;
    Ok(Some((body, end)))
}
```

- `cie_at`: `let mut reader = Cursor { bytes: &bytes[..end], at: body };` ->
  `let mut reader = Reader::starting_at(&bytes[..end], body).map_err(cursor::Error::as_str)?;`
  and `if reader.u32()? != 0 {` -> `if reader.u32().map_err(cursor::Error::as_str)? != 0 {`.
- `elf/image.rs`: `use super::dynamic::{CHUNK, Put, Strings, ...}` drops `Put` and gains
  `use blaze_format::cursor::Put;`; its writer calls rename as above.

The tests (`fdes` against gimli and `reference`, the split walks, "offset 0x0", "where the core
placed an FDE") read only `bad`'s and `walk`'s words, which stay.

> applied by requests (child requests-link-next): the `Put` trait, `Cursor`, the decoder copy and the two constants are gone; writer calls are `put_*` in `elf/dynamic.rs` (versym, verneed, gnu_hash, dynamic, note, `build_hdr`) and `elf/image.rs` (`head`); `walk`, `entry` and `cie_at` as written. The `.eh_frame_hdr` patch (`patch_hdr`) reaches the decoder only through `entry` and `walk`, so it needed no edit of its own; the gimli differentials and patch-equals-build tests call unchanged functions.

### C5. `macho/unwind.rs`: `Reader` goes (needs C3)

- Add `use blaze_format::cursor::{self, Reader};` and
  `use crate::unwind::{cie_encoding, eh_pointer, eh_value};`; remove
  `use blaze_format::leb::LebError;`. Delete `const DW_EH_PE_OMIT`, `const DW_EH_PE_PCREL`,
  `fn cie_encoding`, `fn valid`, `struct Reader` and its `impl`.
- `fdes`: `let mut reader = Reader { bytes, at };` ->
  `let mut reader = Reader::starting_at(bytes, at).map_err(|it| bad(record, it.as_str()))?;`;
  both `.map_err(|it| bad(record, it))?` on `reader.u32()`/`reader.u64()` ->
  `.map_err(|it| bad(record, it.as_str()))?`; `let body = reader.at;` -> `let body = reader.at();`;
  `let mut reader = Reader { bytes: &bytes[..end], at: body };` ->
  `let mut reader = Reader::starting_at(&bytes[..end], body).map_err(|it| bad(record, it.as_str()))?;`;
  `reader.pointer(encoding, region.addr)` -> `eh_pointer(&mut reader, encoding, region.addr)`;
  `reader.value(encoding)` -> `eh_value(&mut reader, encoding)`.
- `Out` (the `__unwind_info` writer over a slice sized up front, `put` at an offset) stays: it
  writes into exact room, not a `Vec`, which `Put` doesn't do.

> applied by requests (child requests-link-next): `Reader`, `cie_encoding`, `valid`, the constants and the `LebError` import are gone. Differs: the parse moved from `fdes` into `record` (also called by the `Last` patch), whose `bad` takes only the words, so the readers map with `|it| bad(it.as_str())`; only `cursor::Reader` is imported (no `cursor::` path is used). "names no CIE" and "unknown version" kept.

### C6. `wasm/reader.rs` and its callers: `Reader` goes, wasm's own reads stay as a trait

`wasm/reader.rs`: delete `struct Reader` and its `impl`, and `use blaze_format::leb::{self,
LebError};`. New, after `use std::ops::Range;`:

```rust
pub(super) use blaze_format::cursor::Reader;

/// What wasm reads beyond the cursor's fields. Its numbers are LEB128 (`uleb32`, `uleb`,
/// `sleb32`, `sleb`), never the cursor's fixed-width `u32`/`u64`/`i32`/`i64`.
pub(super) trait Wasm<'a> {
    /// A count of things that follow, each at least a byte: more than the bytes left is
    /// malformed, and is not allocated for.
    fn count(&mut self) -> Result<usize, String>;
    /// A name: its length, then that many bytes of UTF-8.
    fn name(&mut self) -> Result<&'a str, String>;
    /// `i32.const value; end`, the offset of an active segment of wasm32.
    fn i32_const(&mut self) -> Result<i32, String>;
}

impl<'a> Wasm<'a> for Reader<'a> {
    fn count(&mut self) -> Result<usize, String> {
        let count = self.uleb32()? as usize;
        if count > self.left() {
            // alloc: error path
            return Err(format!("a count of {count} with {} bytes left", self.left()));
        }
        Ok(count)
    }

    fn name(&mut self) -> Result<&'a str, String> {
        let len = self.uleb32()? as usize;
        let bytes = self.take(len)?;
        // alloc: error path
        std::str::from_utf8(bytes).map_err(|_| "a name that is not UTF-8".to_owned())
    }

    fn i32_const(&mut self) -> Result<i32, String> {
        match self.u8()? {
            0x41 => {}
            // alloc: error path
            other => return Err(format!("an offset expression of opcode {other:#04x}")),
        }
        let value = self.sleb32()?;
        match self.u8()? {
            0x0b => Ok(value),
            // alloc: error path
            _ => Err("an offset expression longer than a constant".to_owned()),
        }
    }
}
```

`each_section`, after:

```rust
fn each_section<'a>(bytes: &'a [u8], mut f: impl FnMut(Section<'a>)) -> Result<(), String> {
    let mut reader = Reader::starting_at(bytes, MAGIC.len())?;
    while !reader.is_done() {
        let id = reader.u8()?;
        let len = reader.uleb32()? as usize;
        let at = reader.at();
        // alloc: error path
        let contents = reader.take(len).map_err(|_| format!("section {id} is truncated"))?;
        f(Section { id, contents, at });
    }
    Ok(())
}
```

(`sections` checked `MAGIC` first, so `starting_at` never refuses. The copy's `starting_at`
clamped an `at` past the end; the cursor's refuses it. The callers: this one, and `code.rs`'s
two, which pass positions inside the bytes, so none relied on the clamp.)

The renames, in every file below (the rename of a `.u32()` is the one a build would not catch):

- `.u32()` -> `.uleb32()`, `.u64()` -> `.uleb()`, `.i32()` -> `.sleb32()`, `.i64()` -> `.sleb()`,
  `.done()` -> `.is_done()`, `.bytes(n)` -> `.take(n)`, `.rest()` -> `.take_rest()`, on wasm
  readers only.
- `wasm/reader.rs` tests: `numbers` (lines with `reader.u32()`, `reader.i32()`, `reader.u64()`,
  `reader.done()`), `too_long_or_too_large` (every `.u32()`, `.i32()`, `.i64()`),
  `reads_allocate_nothing` (`r.u32()` twice, `r.i32()`, `r.u64()`, `r.i64()`, `r.bytes(0)`,
  `r.done()`). `names_and_counts` unchanged.
- `wasm/leb.rs` tests, `round_trips`: the two `.u32()` and the two `.i32()`.
- `wasm/code.rs`: import `use super::reader::{Reader, Wasm};`. `.u32()` at `Header::parse`
  (`let len = r.u32()? as usize;`, the two in the type fixups loop, the one in the stack
  pointer loop), `type_fixups` (both), `stack_pointer` (one); `signature`'s `r.bytes(count)?`;
  `parse`'s `if !r.done()`. `Header::parse`: `Reader::starting_at(header, r.at())` ->
  `Reader::starting_at(header, r.at())?`. `Header::reader`:
  `Reader::starting_at(&self.code[..self.body as usize], at as usize).expect(CHECKED)`.
- `wasm/object.rs`: import `use super::reader::{self, Reader, Wasm};`. As of this writing, the
  `.u32()` lines 262, 290, 298, 317, 328, 334, 345, 352, 447, 455, 463, 464, 472, 473, 483, 491,
  516, 520, 528, 533, 562, 571, 572, 573; `.u64()` 409, 411, 529, 530; `.i64()` 574;
  `.rest()` 237; `.bytes(len)` 319, 347, 456; `.done()` 372, 453, 506, 577.
- `wasm/module.rs`: import `use super::reader::{MAGIC, Reader, Wasm};`. `.u32()` lines 1128,
  1155, 1170, 1171, 1183, 1199, 1201, 1211, 1326, 1364, 1365, 1368; `.rest()` 1114;
  `.bytes(len)` 1184, 1202, 1212, 1369 (not `placed.bytes(index)`); `.done()` 295, 341, 1156,
  1172, 1186, 1204, 1209, 1234.

Check after: `rg -n '\.(u32|u64|i32|i64)\(\)' crates/blaze-link/src/wasm/` finds nothing.

`wasm/leb.rs` writers: `name`'s body -> `out.put_bytes_uleb(name.as_bytes());`; `section`'s ->
`out.put_u8(id); out.put_bytes_uleb(contents);` (add `use blaze_format::cursor::Put;`).

> applied by requests (child requests-link-next): as written, every call re-found with rg (code.rs, object.rs, module.rs, leb.rs, reader.rs tests); `rg "\.(u32|u64|i32|i64)\(\)" crates/blaze-link/src/wasm/` finds nothing. `wasm/leb.rs` `name`/`section` on `Put`.

### C7. `stage.rs`: `wasm_imports_in`'s walk

The `(bytes, &mut at)` walk over a wasm code header becomes a `Reader`. Add
`use blaze_format::cursor::{self, Reader};`. Replace `fn uleb`, `fn skip_signature` and
`fn wasm_imports_in` with:

```rust
/// A code header's read error, as the stage says it.
fn in_header(error: cursor::Error) -> String {
    // alloc: error path
    format!("wasm: {error} in a code header")
}

/// Moves past a function type (`0x60`, the parameters, the results).
fn skip_signature(header: &mut Reader<'_>) -> Result<(), String> {
    if header.u8().map_err(in_header)? != 0x60 {
        // alloc: error path
        return Err("wasm: a type that is not a function type".to_owned());
    }
    for _ in 0..2 {
        header.bytes_uleb().map_err(in_header)?;
    }
    Ok(())
}

/// Whether a wasm function's code header ([`blaze_codegen::wasm::Code`]) lists imports:
/// read past its type, types, type fixups and stack pointer fixups without decoding them,
/// so a function without imports allocates nothing.
fn wasm_imports_in(code: &[u8]) -> Result<bool, String> {
    let mut header = Reader::new(Reader::new(code).bytes_uleb().map_err(in_header)?);
    skip_signature(&mut header)?;
    for _ in 0..header.uleb().map_err(in_header)? {
        skip_signature(&mut header)?;
    }
    for _ in 0..header.uleb().map_err(in_header)? {
        header.uleb().map_err(in_header)?;
        header.uleb().map_err(in_header)?;
    }
    for _ in 0..header.uleb().map_err(in_header)? {
        header.uleb().map_err(in_header)?;
    }
    Ok(header.uleb().map_err(in_header)? > 0)
}
```

The test `the_wasm_header_scan_finds_imports_as_the_decoder_does` is unchanged.

> applied by requests (child requests-link-next): as written (`in_header`, `skip_signature` and `wasm_imports_in` on `Reader`).

### C8. `macho/dylib.rs`: the export trie's `uleb`/`cstring`, the test writers

Add `use blaze_format::cursor::Reader;`; delete `use blaze_format::leb::LebError;`,
`fn uleb` and `fn cstring`.

- `trie_exports`, from `let mut at = node;` through the child loop's
  `let child = uleb(data, &mut at)?;`, after:

```rust
        let mut r = Reader::starting_at(data, node)?;
        let terminal = r.uleb()?;
        if terminal != 0 {
            let info = usize::try_from(terminal)
                .ok()
                .and_then(|it| r.take(it).ok())
                // alloc: error path
                .ok_or_else(|| format!("node {node:#x}'s export info runs past the end"))?;
            let (kind, weak) = export(info, &classify)
                // alloc: error path
                .map_err(|what| format!("node {node:#x}: {what}"))?;
            out.push(SharedSymbol { name: super::push_core_name(names, &name)?, version: None, kind, weak });
        }
        // alloc: error path
        let count = r.u8().map_err(|_| format!("node {node:#x} runs past the end"))?;
        let first = stack.len();
        for _ in 0..count {
            let label = r.at();
            let len = r.cstr()?.len();
            let child = r.uleb()?;
```

  (the rest of the loop body unchanged).
- `reference_trie_exports`: the same change, so the differential
  (`trie_walk_matches_reference`, `trie_differential`) still compares equal words:
  `let mut at = node; let terminal = uleb(data, &mut at)?;` ->
  `let mut r = Reader::starting_at(data, node)?; let terminal = r.uleb()?;`; the export info
  `usize::try_from(terminal).ok().and_then(|it| r.take(it).ok()).ok_or_else(..)?` passed to
  `export` as `info`, `at = end;` gone; `let count = r.u8().map_err(|_| format!("node {node:#x}
  runs past the end"))?;` (the `at += 1;` gone); `let label = r.cstr()?;` and
  `let child = r.uleb()?;`.
- `export`, after its `known`:

```rust
    let mut info = Reader::new(info);
    let flags = info.uleb()?;
```

  and below, `uleb(info, &mut at)?` -> `info.uleb()?` (four), `cstring(info, &mut at)?;` ->
  `info.cstr()?;`. Rename the parameter or the local if the `info: &[u8]` shadowing reads
  badly; the shadowing is legal.
- Tests: delete `uleb_limits` (a test of the LEB copy: `blaze_format::leb`'s own tests hold
  `u64::MAX` round-tripping and the overlong refusal; told to `dedup-cursor` to check
  `cursor/tests.rs` has both through `Reader::uleb`). Delete `fn u32le`, `fn u64le`; add
  `use blaze_format::cursor::Put;` to `mod tests`; every `u32le(&mut x, v)` -> `x.put_u32(v)`,
  `u64le(&mut x, v)` -> `x.put_u64(v)` (and `u32le(x, v)` with `x: &mut Vec<u8>` ->
  `x.put_u32(v)`). A missed call fails to build.

The words change from "a ULEB128 at 0x1c runs past the end" to "at byte 28: a LEB128 that ends
early"-style (`cursor::Error`'s `Display`), the same in both walks. `read_allocation_budget`
holds: the cursor allocates nothing.

> applied by requests (child requests-link-next): both walks and `export` on `Reader`, `uleb_limits`/`u32le`/`u64le` gone. Differs: the words do not change. `malformed_tries` checks "too long" (an overlong ULEB128), which the cursor would say as "a LEB128 past the bits of its type", so one `trie_error(cursor::Error)` maps LEB and NUL errors back to the old words ("a ULEB128 at {at:#x} is too long" / "runs past the end", "a string at {at:#x} runs past the end"), used by both walks so the differential compares equal.

### C9. The writers: `macho.rs` `Out`, `pe/image.rs` `Buf`, `symbols.rs` `write_key`

Apply after `requests-link-next-writers`' items in `macho.rs`.

- `macho.rs`: add `use blaze_format::cursor::Put;`. In `impl Out`, delete `u8`, `u16`, `u32`,
  `u64`; keep `name16` (`align` goes too: C10 moves it onto `Put::pad_to`). Add, after the
  `impl`:

```rust
impl std::ops::Deref for Out {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl std::ops::DerefMut for Out {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}
```

  and every `x.u8(v)`/`.u16(v)`/`.u32(v)`/`.u64(v)` on an `Out` (or `&mut Out`, as in
  `path_command`) -> `x.put_u8(v)`/`put_u16`/`put_u32`/`put_u64`. A missed one fails to build.
- `pe/image.rs`: add `use blaze_format::cursor::Put;`. In `impl Buf`, delete `u8`, `u16`, `u32`,
  `u64`, `bytes`; `string`'s body -> `self.0.put_bytes(text.as_bytes()); self.0.put_u8(0);`
  (not `put_cstr`, which panics on a NUL inside; the copy wrote it through); keep
  `with_capacity`, `string`, `len` (`align` per C10). Add the same `Deref`/`DerefMut` to `Vec<u8>` for
  `Buf`. Every writer call on a `Buf` in `pe/image.rs`, `pe.rs` and `pe/tests.rs` renames as
  above, and `.bytes(b)` -> `.put_bytes(b)` (with `use blaze_format::cursor::Put;` in
  `pe/tests.rs` and `pe.rs` where they call them). `Buf::len` (a `u64`) shadows `Vec::len` as
  before.
- `symbols.rs` `write_key`: the closures `name` and `word` go; add
  `use blaze_format::cursor::Put;`; `name(out, a)` -> `out.put_bytes_u64(self.str(a).as_bytes())`,
  `word(out, x)` -> `out.put_u32(x)` (same bytes: a `u64` length, then the name; a `u32`).

> applied by requests (child requests-link-next): `Out` and `Buf` deref to `Vec<u8>` with their `u*`/`bytes` copies gone (79 calls in macho.rs, 90 + 7 in pe/image.rs, 48 + 11 in pe/tests.rs; pe.rs has none); `write_key` on `Put` (coverage keys kept, same bytes). Also: `macho/chained.rs` `fixups` (new since the request) drops its `put32` closure for `put_u16`/`put_u32`/`put_u64`/`patch_u32`, and `__LINKEDIT`s 8-byte padding in `append` is `pad_to(8)`.

### Kept on purpose (no change)

- Reads and patches at fixed offsets, not cursors: `reloc.rs` (relocation writes into placed
  bytes), `elf/native.rs`, `table.rs`, `file.rs`, `write.rs`, `native.rs`, `live.rs` `word`,
  `elf/image.rs` `Shdr::bytes` (a fixed record into an array) and `verneeds` (a `vn_next`
  chain at `+12`), `pe/coff.rs` `u16_at`/`u32_at`/`u64_at` (random access into COFF headers),
  `macho/tbd.rs` and `macho/unwind.rs` `u32_at`/`u64_at`, `macho/archive.rs`'s ranlib patch,
  `wasm/reloc.rs` `put`, `wasm/module.rs`'s slot read, `elf/read/strings.rs` (SWAR).
- Inline appends with no helper (`dwarf.rs`, `macho/archive.rs`, `wasm/object.rs` tests): no
  copy to delete.
- `pe/coff.rs` `Reader` and `macho/input.rs` `Reader`: object readers over `object`'s parsed
  headers, not byte cursors.
- `src/tests.rs` `at`/`u32`/`u64` readers: test reads of a linked image at addresses.

## dedup: byte cursor onto blaze_format::cursor (addendum C10: `align` -> `Put::pad_to`)

From `dedup-cursor`, adding to C4 and C9 above: `blaze_format::cursor::Put` now has
`fn pad_to(&mut self, multiple: usize)` (`self.resize(self.len().next_multiple_of(multiple), 0)`),
the body all three writers' `align` had. With C9's `Deref`/`DerefMut` to `Vec<u8>`, `Out` and
`Buf` get it through `Put`.

- `macho.rs`: delete `Out::align` (~789); `linkedit.align(16)` (~1244) -> `linkedit.pad_to(16)`,
  `strings.align(8)` (~1601) -> `strings.pad_to(8)`.
- `pe/image.rs`: delete `Buf::align` (~100); `out.align(8)` (~503) -> `out.pad_to(8)`,
  `out.align(2)` (~521) -> `out.pad_to(2)`.
- `pe/tests.rs`: `buf.align(8)` (~801) and the three `out.align(2)` (~1293, ~1306, ~1310) ->
  `pad_to`, with `use blaze_format::cursor::Put;` (C9 adds it there already).
- `elf/dynamic.rs`: C4 deletes its `Put` trait with `align` (no caller); nothing more.

Same bytes. A missed `.align(` on an `Out` or `Buf` fails to build.

> forwarded to requests-link-next (requests, pass 21; told by tmux).
> applied by requests (child requests-link-next) for `elf/dynamic.rs`: its `Put` with `align` (no caller) went with C4; nothing more there. The `macho.rs` and `pe` parts belong to `requests-link-next-cursor-rest`.
> applied by requests (child requests-link-next): macho.rs and pe parts: `Out::align`/`Buf::align` gone, `linkedit`/`strings`/`out`/`buf` `.align(n)` -> `.pad_to(n)` (2 in macho.rs, 2 in pe/image.rs, 4 in pe/tests.rs). The elf part is requests-link-next-cursor-eh's.
