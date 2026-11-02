//! A code piece's header ([`blaze_codegen::wasm::Code::encode`]) read in place: where each part
//! of it starts in the piece, nothing copied or allocated. [`Header::parse`] checks it all
//! once, so the parts are then walked without checks.

use blaze_codegen::wasm::ValType;

use super::leb::{self, uleb, uleb_len};
use super::reader::{Reader, Wasm};

/// 48 bytes on a 64-bit host: the piece and seven `u32` offsets into it. Reading a part walks
/// it from its offset, O(its length): a function's lists are a few entries.
#[derive(Clone, Copy, Debug)]
pub(super) struct Header<'a> {
    code: &'a [u8],
    ty: u32,
    types: u32,
    type_fixups: u32,
    stack_pointer: u32,
    imports: u32,
    export: u32,
    body: u32,
}

const _: () = assert!(size_of::<Header<'static>>() <= 48);

/// An encoded function type (`0x60`, the parameters, the results) as the type section has it:
/// the key types are interned by.
pub(super) type Sig<'a> = &'a [u8];

/// `fn()`.
pub(super) const NOTHING: Sig<'static> = &[0x60, 0, 0];

/// Moves past one encoded function type, checking it, and says where it was.
pub(super) fn signature<'a>(r: &mut Reader<'a>, bytes: &'a [u8]) -> Result<Sig<'a>, String> {
    let start = r.at();
    if r.u8()? != 0x60 {
        // alloc: error path
        return Err("a type that is not a function type".to_owned());
    }
    for _ in 0..2 {
        let count = r.count()?;
        if let Some(&byte) = r.take(count)?.iter().find(|&&it| ValType::from_byte(it).is_none()) {
            // alloc: error path
            return Err(format!("the value type {byte:#04x}"));
        }
    }
    Ok(&bytes[start..r.at()])
}

const CHECKED: &str = "checked by Header::parse";

/// A header from borrowed parts, for the functions the object reader takes out of C objects:
/// the bytes [`blaze_codegen::wasm::Code::encode`] writes for a `Code` of these parts with no
/// `__stack_pointer` places and no imports (an object names those through relocations and
/// symbols). Nothing is decoded into a `Signature`, and the code is appended to a buffer the
/// caller sized ([`Parts::len`]).
pub(super) struct Parts<'a> {
    pub ty: Sig<'a>,
    pub types: &'a [Sig<'a>],
    pub type_fixups: &'a [(u32, u32)],
    pub export: Option<&'a str>,
}

impl Parts<'_> {
    fn header_len(&self) -> usize {
        let types: usize = self.types.iter().map(|it| it.len()).sum();
        let fixups: usize = self
            .type_fixups
            .iter()
            .map(|&(at, which)| uleb_len(at.into()) + uleb_len(which.into()))
            .sum();
        let export = self.export.map_or(1, |it| 1 + uleb_len(it.len() as u64) + it.len());
        // The two empty lists, the `__stack_pointer` places and the imports, are a 0 each.
        self.ty.len()
            + uleb_len(self.types.len() as u64)
            + types
            + uleb_len(self.type_fixups.len() as u64)
            + fixups
            + 2
            + export
    }

    /// The length of the code [`Parts::encode_into`] writes for a body of `body` bytes.
    pub(super) fn len(&self, body: usize) -> usize {
        let header = self.header_len();
        uleb_len(header as u64) + header + body
    }

    /// The function's code, the header and then `body`, and where `body` starts in it.
    #[cfg(test)]
    pub(super) fn encode(&self, body: &[u8]) -> (Vec<u8>, u32) {
        let mut code = Vec::with_capacity(self.len(body.len()));
        let start = self.encode_into(&mut code, body);
        (code, start)
    }

    /// Appends the function's code, the header and then `body`, to `out` (which has room for
    /// [`Parts::len`] more bytes), and says where `body` starts in it.
    pub(super) fn encode_into(&self, out: &mut Vec<u8>, body: &[u8]) -> u32 {
        let from = out.len();
        let header = self.header_len();
        let start = uleb_len(header as u64) + header;
        uleb(out, header as u64);
        out.extend_from_slice(self.ty);
        uleb(out, self.types.len() as u64);
        for ty in self.types {
            out.extend_from_slice(ty);
        }
        uleb(out, self.type_fixups.len() as u64);
        for &(at, which) in self.type_fixups {
            uleb(out, at.into());
            uleb(out, which.into());
        }
        out.extend_from_slice(&[0, 0]);
        match self.export {
            None => out.push(0),
            Some(name) => {
                out.push(1);
                leb::name(out, name);
            }
        }
        debug_assert_eq!(out.len() - from, start);
        out.extend_from_slice(body);
        start as u32
    }
}

impl<'a> Header<'a> {
    /// Reads where the parts of `code`'s header are, checking every one of them.
    pub(super) fn parse(code: &'a [u8]) -> Result<Header<'a>, String> {
        if u32::try_from(code.len()).is_err() {
            // alloc: error path
            return Err("a function of 4 GiB or more".to_owned());
        }
        let mut r = Reader::new(code);
        let len = r.uleb32()? as usize;
        let end = r
            .at()
            .checked_add(len)
            .filter(|&end| end <= code.len())
            .ok_or("a truncated code header")?;
        let header = &code[..end];
        let mut r = Reader::starting_at(header, r.at())?;
        let at = |r: &Reader<'_>| r.at() as u32;
        let ty = at(&r);
        signature(&mut r, header)?;
        let types = at(&r);
        for _ in 0..r.count()? {
            signature(&mut r, header)?;
        }
        let type_fixups = at(&r);
        for _ in 0..r.count()? {
            r.uleb32()?;
            r.uleb32()?;
        }
        let stack_pointer = at(&r);
        for _ in 0..r.count()? {
            r.uleb32()?;
        }
        let imports = at(&r);
        for _ in 0..r.count()? {
            r.name()?;
            r.name()?;
            r.name()?;
            signature(&mut r, header)?;
        }
        let export = at(&r);
        match r.u8()? {
            0 => {}
            1 => {
                r.name()?;
            }
            // alloc: error path
            tag => return Err(format!("a code header's export tag {tag}")),
        }
        if !r.is_done() {
            // alloc: error path
            return Err("a code header longer than what it holds".to_owned());
        }
        let body = end as u32;
        Ok(Header { code, ty, types, type_fixups, stack_pointer, imports, export, body })
    }

    fn reader(&self, at: u32) -> Reader<'a> {
        Reader::starting_at(&self.code[..self.body as usize], at as usize).expect(CHECKED)
    }

    /// The piece's length: the header and the body.
    pub(super) fn len(&self) -> usize {
        self.code.len()
    }

    /// The function's type.
    pub(super) fn ty(&self) -> Sig<'a> {
        &self.code[self.ty as usize..self.types as usize]
    }

    /// The locals and instructions, as the code section has them after the size.
    pub(super) fn body(&self) -> &'a [u8] {
        &self.code[self.body as usize..]
    }

    /// How many types its `call_indirect`s name, and how many places in the body take one.
    pub(super) fn counts(&self) -> (usize, usize) {
        let types = self.reader(self.types).count().expect(CHECKED);
        (types, self.reader(self.type_fixups).count().expect(CHECKED))
    }

    /// The types its `call_indirect`s name, in order, into `out` (cleared first).
    pub(super) fn types(&self, out: &mut Vec<Sig<'a>>) {
        out.clear();
        let header = &self.code[..self.body as usize];
        let mut r = self.reader(self.types);
        for _ in 0..r.count().expect(CHECKED) {
            out.push(signature(&mut r, header).expect(CHECKED));
        }
    }

    /// Each place in the body that takes a type index, and which of [`Header::types`] it is.
    pub(super) fn type_fixups(&self) -> impl Iterator<Item = (u32, u32)> + 'a {
        let mut r = self.reader(self.type_fixups);
        let count = r.count().expect(CHECKED);
        (0..count).map(move |_| (r.uleb32().expect(CHECKED), r.uleb32().expect(CHECKED)))
    }

    /// Each place in the body that takes the index of `__stack_pointer`.
    pub(super) fn stack_pointer(&self) -> impl Iterator<Item = u32> + 'a {
        let mut r = self.reader(self.stack_pointer);
        let count = r.count().expect(CHECKED);
        (0..count).map(move |_| r.uleb32().expect(CHECKED))
    }

    /// Each extern it names and what that is imported as: the name, the module, the field and
    /// the type.
    pub(super) fn imports(&self) -> impl Iterator<Item = (&'a str, &'a str, &'a str, Sig<'a>)> + 'a {
        let header = &self.code[..self.body as usize];
        let mut r = self.reader(self.imports);
        let count = r.count().expect(CHECKED);
        (0..count).map(move |_| {
            let name = r.name().expect(CHECKED);
            let module = r.name().expect(CHECKED);
            let field = r.name().expect(CHECKED);
            (name, module, field, signature(&mut r, header).expect(CHECKED))
        })
    }

    /// The name the module exports it under, whatever the output's exports say.
    pub(super) fn export(&self) -> Option<&'a str> {
        let mut r = self.reader(self.export);
        match r.u8().expect(CHECKED) {
            0 => None,
            _ => Some(r.name().expect(CHECKED)),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Code, Import, Signature};

    use super::*;

    fn encoded(signature: &Signature) -> Vec<u8> {
        let mut out = Vec::new();
        signature.encode(&mut out);
        out
    }

    #[test]
    fn reads_what_codegen_writes() {
        let call = Signature { params: vec![ValType::V128], results: vec![] };
        let write = Signature { params: vec![ValType::I32; 4], results: vec![ValType::I32] };
        let code = Code {
            ty: Signature { params: vec![ValType::I32, ValType::I64], results: vec![ValType::F32] },
            types: vec![Signature::default(), call.clone()],
            type_fixups: vec![(3, 1), (200, 0)],
            stack_pointer: vec![9, 40],
            imports: vec![(
                "fd_write".to_owned(),
                Import {
                    module: "wasi_snapshot_preview1".to_owned(),
                    field: "fd_write".to_owned(),
                    ty: write.clone(),
                },
            )],
            export: Some("_start".to_owned()),
        };
        let body = [0x00, 0x01, 0x0b];
        let (bytes, start) = code.encode(&body);
        let header = Header::parse(&bytes).unwrap();
        assert_eq!(header.ty(), encoded(&code.ty));
        assert_eq!(header.body(), body);
        assert_eq!(header.body, start);
        let mut types = Vec::new();
        header.types(&mut types);
        assert_eq!(types, [NOTHING.to_vec(), encoded(&call)]);
        assert_eq!(header.type_fixups().collect::<Vec<_>>(), [(3, 1), (200, 0)]);
        assert_eq!(header.stack_pointer().collect::<Vec<_>>(), [9, 40]);
        let imports: Vec<_> = header.imports().collect();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].0, "fd_write");
        assert_eq!((imports[0].1, imports[0].2), ("wasi_snapshot_preview1", "fd_write"));
        assert_eq!(imports[0].3, encoded(&write));
        assert_eq!(header.export(), Some("_start"));

        let plain = Code::default().encode(&body).0;
        let header = Header::parse(&plain).unwrap();
        assert_eq!(header.ty(), NOTHING);
        assert_eq!(header.export(), None);
        assert_eq!(header.type_fixups().count() + header.stack_pointer().count(), 0);
        assert_eq!(header.imports().count(), 0);
    }

    #[test]
    fn parts_encode_as_codegen_does() {
        let call = Signature { params: vec![ValType::V128], results: vec![] };
        let ty = Signature { params: vec![ValType::I32], results: vec![ValType::F64] };
        let body = [0x00, 0x0b];
        let long = "x".repeat(200);
        for export in [None, Some("_start"), Some(long.as_str())] {
            let code = Code {
                ty: ty.clone(),
                types: vec![Signature::default(), call.clone()],
                type_fixups: vec![(3, 1), (300, 0)],
                export: export.map(str::to_owned),
                ..Code::default()
            };
            let (ty, types) = (encoded(&ty), [encoded(&Signature::default()), encoded(&call)]);
            let types = [types[0].as_slice(), types[1].as_slice()];
            let parts =
                Parts { ty: &ty, types: &types, type_fixups: &code.type_fixups, export };
            assert_eq!(parts.encode(&body), code.encode(&body));
        }
    }

    #[test]
    fn refuses_a_bad_header() {
        let (bytes, start) = Code::default().encode(&[0x0b]);
        assert!(Header::parse(&bytes[..start as usize - 1]).is_err());
        let mut bad = bytes.clone();
        bad[1] = 0x5f;
        assert!(Header::parse(&bad).unwrap_err().contains("function type"));
        let mut bad = bytes.clone();
        let export = start as usize - 1;
        bad[export] = 7;
        assert!(Header::parse(&bad).unwrap_err().contains("export tag 7"));
        let mut long = bytes.clone();
        long[0] += 1;
        long.insert(start as usize, 0);
        assert!(Header::parse(&long).unwrap_err().contains("longer"));
    }
}
