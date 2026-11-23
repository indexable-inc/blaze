//! A dependency macro supplies the const-generic argument of an array-returning
//! function. The defining crate supplies the function, and the expansion's
//! block supplies its input. Both must be visible during constant evaluation.

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_drive::{Kind, Name, Tool};
use blaze_engine::{Answer, Buffer, Cause, CellId, Done, Edit, Engine, Everything, Phase, Planned, Plant, Stage, Stands, Tools};
use blaze_ladder::crates::{Toolchain, syntax_of};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records as Check};
use blaze_ladder::front2::consts::{ConstFail, Consts, RequiredBodyCause};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld, Refused};
use blaze_ladder::front2::project_records::BodySource;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_lower::scoped::Prepares;
use blaze_mem::{Frame, tag::Tag};
use blaze_own::Alloc;
use blaze_scope::expansion_origins::{OriginsError, split};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names_cells::NameCell;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::{items_record, names_record, task::Serial};

const ROOT: &str = r#"
pub const fn in_body() -> [u8; 2] { let x: [u8; 2] = dep::hex!("00ff"); x }
pub const BYTES: [u8; 2] = dep::hex!("00ff");
pub const TWO_LITERALS: [u8; 2] = dep::hex!("00" "ff");
pub static STATIC_BYTES: [u8; 2] = dep::hex!("00ff");
const fn take(x: [u8; 2]) -> [u8; 2] { x }
pub const fn argument() -> [u8; 2] { take(dep::hex!("00ff")) }
pub const FROM_BODY: [u8; 2] = in_body();
pub const FROM_ARGUMENT: [u8; 2] = argument();
"#;

// hex-literal 1.1.0, src/lib.rs.
const DEP: &str = r#"// SPDX-License-Identifier: MIT OR Apache-2.0
#![no_std]
const fn next_hex_char(string: &[u8], mut pos: usize) -> Option<(u8, usize)> {
    while pos < string.len() {
        let raw_val = string[pos];
        pos += 1;
        let val = match raw_val {
            b'0'..=b'9' => raw_val - 48,
            b'A'..=b'F' => raw_val - 55,
            b'a'..=b'f' => raw_val - 87,
            b' ' | b':' | b'\r' | b'\n' | b'\t' => continue,
            0..=127 => panic!("Encountered invalid ASCII character"),
            _ => panic!("Encountered non-ASCII character"),
        };
        return Some((val, pos));
    }
    None
}

const fn next_byte(string: &[u8], pos: usize) -> Option<(u8, usize)> {
    let (half1, pos) = match next_hex_char(string, pos) {
        Some(v) => v,
        None => return None,
    };
    let (half2, pos) = match next_hex_char(string, pos) {
        Some(v) => v,
        None => panic!("Odd number of hex characters"),
    };
    Some(((half1 << 4) + half2, pos))
}

/// Compute length of a byte array which will be decoded from the strings.
///
/// This function is an implementation detail and SHOULD NOT be called directly!
#[doc(hidden)]
pub const fn len(strings: &[&[u8]]) -> usize {
    let mut i = 0;
    let mut len = 0;
    while i < strings.len() {
        let mut pos = 0;
        while let Some((_, new_pos)) = next_byte(strings[i], pos) {
            len += 1;
            pos = new_pos;
        }
        i += 1;
    }
    len
}

/// Decode hex strings into a byte array of pre-computed length.
///
/// This function is an implementation detail and SHOULD NOT be called directly!
#[doc(hidden)]
pub const fn decode<const LEN: usize>(strings: &[&[u8]]) -> Option<[u8; LEN]> {
    let mut string_pos = 0;
    let mut buf = [0u8; LEN];
    let mut buf_pos = 0;
    while string_pos < strings.len() {
        let mut pos = 0;
        let string = &strings[string_pos];
        string_pos += 1;

        while let Some((byte, new_pos)) = next_byte(string, pos) {
            buf[buf_pos] = byte;
            buf_pos += 1;
            pos = new_pos;
        }
    }
    if LEN == buf_pos { Some(buf) } else { None }
}

/// Converts a sequence of hexadecimal string literals to a byte array at compile time.
///
/// See the crate-level docs for more information.
#[macro_export]
macro_rules! hex {
    ($($s:literal)*) => {{
        const STRINGS: &[&'static [u8]] = &[$($s.as_bytes(),)*];
        const {
            $crate::decode::<{ $crate::len(STRINGS) }>(STRINGS)
                .expect("Output array length should be correct")
        }
    }};
}
"#;

// alloy-primitives 1.7.3, src/hex_literal.rs.
const ALLOY_HEX: &str = r#"// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hex literal macro implementation.
//!
//! Modified from the [`hex-literal`](https://github.com/RustCrypto/utils/tree/master/hex-literal)
//! crate to allow `0x` prefixes.

const fn next_hex_char(string: &[u8], mut pos: usize) -> Option<(u8, usize)> {
    while pos < string.len() {
        let raw_val = string[pos];
        pos += 1;
        let val = match raw_val {
            b'0'..=b'9' => raw_val - 48,
            b'A'..=b'F' => raw_val - 55,
            b'a'..=b'f' => raw_val - 87,
            b' ' | b'\r' | b'\n' | b'\t' => continue,
            0..=127 => panic!("Encountered invalid ASCII character"),
            _ => panic!("Encountered non-ASCII character"),
        };
        return Some((val, pos));
    }
    None
}

const fn next_byte(string: &[u8], pos: usize) -> Option<(u8, usize)> {
    let (half1, pos) = match next_hex_char(string, pos) {
        Some(v) => v,
        None => return None,
    };
    let (half2, pos) = match next_hex_char(string, pos) {
        Some(v) => v,
        None => panic!("Odd number of hex characters"),
    };
    Some(((half1 << 4) + half2, pos))
}

/// Strips the `0x` prefix from a hex string.
///
/// This function is an implementation detail and SHOULD NOT be called directly!
#[doc(hidden)]
pub const fn strip_hex_prefix(string: &[u8]) -> &[u8] {
    if let [b'0', b'x' | b'X', rest @ ..] = string { rest } else { string }
}

/// Compute length of a byte array which will be decoded from the strings.
///
/// This function is an implementation detail and SHOULD NOT be called directly!
#[doc(hidden)]
pub const fn len(strings: &[&[u8]]) -> usize {
    let mut i = 0;
    let mut len = 0;
    while i < strings.len() {
        let mut pos = 0;
        while let Some((_, new_pos)) = next_byte(strings[i], pos) {
            len += 1;
            pos = new_pos;
        }
        i += 1;
    }
    len
}

/// Decode hex strings into a byte array of pre-computed length.
///
/// This function is an implementation detail and SHOULD NOT be called directly!
#[doc(hidden)]
pub const fn decode<const LEN: usize>(strings: &[&[u8]]) -> [u8; LEN] {
    let mut i = 0;
    let mut buf = [0u8; LEN];
    let mut buf_pos = 0;
    while i < strings.len() {
        let mut pos = 0;
        while let Some((byte, new_pos)) = next_byte(strings[i], pos) {
            buf[buf_pos] = byte;
            buf_pos += 1;
            pos = new_pos;
        }
        i += 1;
    }
    if LEN != buf_pos {
        panic!("Length mismatch. Please report this bug.");
    }
    buf
}

/// Macro for converting sequence of string literals containing hex-encoded data
/// into an array of bytes.
#[macro_export]
macro_rules! hex {
    ($($s:literal)*) => {const {
        const STRINGS: &[&[u8]] = &[$( $crate::hex_literal::strip_hex_prefix($s.as_bytes()), )*];
        $crate::hex_literal::decode::<{ $crate::hex_literal::len(STRINGS) }>(STRINGS)
    }};
}
#[doc(hidden)] // Use `crate::hex` directly instead!
pub use crate::hex;
"#;

// Only the types and constructors exercised by these macro invocations are needed here.
// The macro generator and its invocation are from alloy-primitives 1.7.3.
const ALLOY_DEP: &str = r#"// SPDX-License-Identifier: MIT OR Apache-2.0
#![no_std]
#[doc(hidden)] // Use `hex` directly instead!
pub mod hex_literal;

pub struct FixedBytes<const N: usize>(pub [u8; N]);
impl<const N: usize> FixedBytes<N> {
    pub const ZERO: Self = Self([0u8; N]);
    pub const fn new(bytes: [u8; N]) -> Self { Self(bytes) }
}
pub type B256 = FixedBytes<32>;
pub struct Address(pub FixedBytes<20>);
impl Address {
    pub const ZERO: Self = Self(FixedBytes::ZERO);
    pub const fn new(bytes: [u8; 20]) -> Self { Self(FixedBytes(bytes)) }
}

macro_rules! fixed_bytes_macros {
    ($d:tt $($(#[$attr:meta])* macro $name:ident($ty:ident $($rest:tt)*);)*) => {$(
        /// Converts a sequence of string literals containing hex-encoded data
        #[doc = concat!(
            "into a new [`", stringify!($ty), "`][crate::", stringify!($ty), "] at compile time.\n",
        )]
        ///
        /// If the input is empty, a zero-initialized array is returned.
        ///
        /// See [`hex!`](crate::hex!) for more information.
        ///
        /// # Examples
        ///
        /// ```
        #[doc = concat!("use alloy_primitives::{", stringify!($name), ", ", stringify!($ty), "};")]
        ///
        #[doc = concat!("const ZERO: ", stringify!($ty $($rest)*), " = ", stringify!($name), "!();")]
        #[doc = concat!("assert_eq!(ZERO, ", stringify!($ty), "::ZERO);")]
        ///
        /// # stringify!(
        #[doc = concat!("let byte_array: ", stringify!($ty), " = ", stringify!($name), "!(\"0x0123abcd…\");")]
        /// # );
        /// ```
        $(#[$attr])*
        #[macro_export]
        macro_rules! $name {
            () => {
                $crate::$ty::ZERO
            };

            ($d ($d t:tt)+) => {
                $crate::$ty::new($crate::hex!($d ($d t)+))
            };
        }
    )*};
}

fixed_bytes_macros! { $
    macro address(Address);

    macro b64(B64);

    macro b128(B128);

    macro b256(B256);

    macro b512(B512);

    macro bloom(Bloom);

    macro fixed_bytes(FixedBytes<0>); // <0> is just for the doctest
}
"#;

const ALLOY_CORE: &str = "#![no_std]\npub use alloy_primitives as primitives;";
const ALLOY: &str = "#![no_std]\npub use alloy_core as core;\npub use self::core::primitives;";

const ALLOY_CALLS: &str = r#"
pub const REEXPORTED: [u8; 2] = dep::hex_literal::hex!("0x00" "0Xff");
pub const fn inbox_token() -> dep::Address {
    let token = dep::address!("0x20c00000000000000000000000000000000000cc");
    token
}
pub const TOKEN: dep::Address = inbox_token();
pub const HASH: dep::B256 = dep::b256!("00000000000000000000000000000000000000000000000000000000000000ff");
pub const PREFIX: dep::FixedBytes<2> = dep::fixed_bytes!("00" "ff");
"#;

struct Files<'a> {
    app: &'a str,
    dep: &'a str,
    frame: &'a Frame,
}

impl<'a> Files<'a> {
    fn library(&self, path: &str) -> Option<&'a [u8]> {
        let relative = std::path::Path::new(path.strip_prefix("/library/")?);
        assert!(relative.components().all(|p| matches!(p, std::path::Component::Normal(_))), "{path}");
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library").join(relative);
        match std::fs::read(&file) {
            Ok(bytes) => Some(self.frame.alloc_slice_copy(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("reading {}: {e}", file.display()),
        }
    }
}

impl<'a> SourceTexts<'a> for Files<'a> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        match (krate, path) {
            (0, "/app/src/lib.rs") => Some(self.app),
            (1, "/dep/src/lib.rs") => Some(self.dep),
            (1, "/dep/src/hex_literal.rs") => Some(ALLOY_HEX),
            (2, _) => Some(std::str::from_utf8(self.library(path)?).unwrap_or_else(|e| panic!("{path}: {e}"))),
            (3, "/facade/src/lib.rs") => Some(ALLOY_CORE),
            (4, "/alloy/src/lib.rs") => Some(ALLOY),
            _ => None,
        }
    }

    fn read_data(&self, krate: usize, _: &str, path: &str) -> Option<&'a [u8]> {
        match krate {
            2 => self.library(path),
            _ => self.read(krate, path).map(str::as_bytes),
        }
    }
}

struct CrateRecords<'a> {
    names: &'a [u8],
    items: &'a [u8],
    files: Vec<(&'a str, &'a str)>,
    expansions: Vec<(ItemKey, &'a str, &'a [u8])>,
}

struct Saved<'a>(Vec<CrateRecords<'a>>);

impl<'a> Saved<'a> {
    fn new<'n: 'a>(names: &Names<'n>, keep: blaze_own::Bump<'a>, scratch: Alloc<'_>) -> Self {
        Self(names.crates().iter().map(|c| CrateRecords {
            names: aligned(&names_record::encode(c, scratch).expect("names record"), keep),
            items: aligned(&items_record::encode_with(c, names, scratch).expect("items record"), keep),
            files: c.files.iter().map(|f| (f.package.as_str(), f.text)).collect(),
            expansions: c.expansions.iter().filter_map(|(key, row)| {
                let made = row.made?;
                let origins: &[u8] = match &made.origins {
                    Ok(bytes) => *bytes,
                    Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
                    Err(e) => panic!("origins of expansion {key:?}: {e:?}"),
                };
                Some((*key, made.text, origins))
            }).collect(),
        }).collect())
    }
}

impl<'a, 'data: 'a> Load<'a> for Saved<'data> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> { Some(self.0.get(krate)?.names) }
    fn items(&self, krate: usize) -> Option<&'a [u8]> { Some(self.0.get(krate)?.items) }
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        self.0.get(krate)?.files.iter().find(|(p, _)| *p == path.as_str()).map(|row| row.1)
    }
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'a>> {
        let row = self.0.get(krate)?.expansions.iter().find(|row| row.0 == call)?;
        let bytes = row.2;
        let origins = if bytes.is_empty() { (&[][..], &[][..]) } else { split(bytes).expect("saved expansion origins") };
        Some(LoadedExpansion { text: row.1, origins: Some(origins) })
    }
}

fn item<'n, S: NamesSource<'n>>(names: S, krate: usize, path: &[&str]) -> ItemKey {
    let target = Paths { names }.path(names.root_module(krate).expect("crate root"), false, path, Ns::Value)
        .unwrap_or_else(|e| panic!("{path:?}: {e:?}"));
    assert_eq!(target.consumed as usize, path.len(), "{path:?}");
    target.key
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    HexLiteral,
    Alloy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Valid,
    Longer,
    MissingInput,
    InvalidLen,
    InvalidStrings,
}

fn check<'n, S: BodySource<'n>>(world: &mut ProjectWorld<'n, S>, provider: Provider, case: Case, label: &str, arena: blaze_own::Bump<'n>) {
    let names = world.front.names;
    let wrappers: &[&str] = match provider {
        Provider::HexLiteral => &[],
        Provider::Alloy => &["REEXPORTED", "TOKEN", "HASH", "PREFIX"],
    };
    let inbox = (provider == Provider::Alloy).then_some("inbox_token");
    for name in ["in_body", "BYTES", "STATIC_BYTES", "argument", "TWO_LITERALS"].into_iter().chain(inbox).chain(wrappers.iter().copied()) {
        let result = world.check(BodyRef::Free(item(names, 0, &[name])), arena.alloc());
        match case {
            Case::Valid => assert!(result.is_clean() && result.undecided.is_empty(), "{label} {name}: {result:?}"),
            Case::Longer => {
                assert!(result.refused.is_none() && result.gaps.is_empty(), "{label} {name}: {result:?}");
                assert!(result.diagnostics.iter().any(|d| d.code == "E0308"), "{label} {name}: {result:?}");
            }
            Case::MissingInput => {
                assert!(result.refused.is_none(), "{label} {name}: a missing block input is a path gap, got refusal {result:?}");
                assert!(result.diagnostics.is_empty(), "{label} {name}: the unresolved path has no completed-body diagnostics: {result:?}");
                assert!(result.gaps.iter().any(|gap| {
                    let shown = gap.to_string();
                    shown.contains("a path in module") && shown.contains("no segments")
                }), "{label} {name}: the macro block's missing STRINGS stays a typed path gap: {result:?}");
            }
            Case::InvalidLen | Case::InvalidStrings => {
                let Some(Refused::Length(error)) = &result.refused else {
                    panic!("{label} {name}: expected the required constant's typed refusal, got {result:?}")
                };
                let ConstFail::RequiredBody { constant: root, needed, shown, cause, .. } = error else {
                    panic!("{label} {name}: expected the anonymous body's diagnostic, got {error}")
                };
                let RequiredBodyCause::Check(CheckFail::Diags { owner, diags, .. }) = cause else {
                    panic!("{label} {name}: {cause}")
                };
                let missing = match case {
                    Case::InvalidLen => {
                        assert_ne!(root, needed);
                        assert!(shown.as_deref().is_some_and(|n| n.contains("len")), "{error}");
                        "MISSING_LENGTH"
                    }
                    Case::InvalidStrings => {
                        assert_ne!(root, needed);
                        assert_eq!(shown.as_deref(), Some("STRINGS"));
                        "MISSING_BYTES"
                    }
                    _ => unreachable!(),
                };
                assert_eq!(needed, owner);
                assert!(diags.iter().any(|(code, _, _)| code.code() == "E0425"), "{label} {name}: {error}");
                let diagnostic = cause.to_string();
                assert!(diagnostic.contains(missing) && error.to_string().contains(&diagnostic), "{label} {name}: {error}");
                let words = blaze_ladder::front2::said::refused_words(names, result.refused.as_ref().expect("body refusal"));
                assert!(words.contains(&diagnostic), "{label} {name}: {words}");
            }
        }
    }
    if case != Case::Valid { return; }
    let mut consts = Consts::default();
    let prepared = Prepares::new_in(arena);
    let mut store = Store::new(world.front.index.lang("sized"), world.front.frame, blaze_own::testing::heap().alloc());
    store.pointers = blaze_ladder::front2::receiver::Pointers::find(&world.front.index, world.front.resolver);
    let mut buckets = None;
    let mut check = Check { names, keys: &mut world.front.keys, index: &world.front.index, store: &mut store,
        buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut token = [0; 20];
    token[..2].copy_from_slice(&[0x20, 0xc0]);
    token[19] = 0xcc;
    let mut hash = [0; 32];
    hash[31] = 0xff;
    for name in ["FROM_BODY", "BYTES", "STATIC_BYTES", "FROM_ARGUMENT", "TWO_LITERALS"].into_iter().chain(wrappers.iter().copied()) {
        let record = consts.value(&mut check, prepared, checker_of(item(names, 0, &[name]))).unwrap_or_else(|e| panic!("{label} {name}: {e}"));
        let root = record.data.allocs.first().expect("array allocation");
        assert!(root.relocs.is_empty(), "{label} {name}: unexpected reference in a byte array");
        let expected: &[u8] = match name {
            "TOKEN" => &token,
            "HASH" => &hash,
            _ => &[0x00, 0xff],
        };
        assert_eq!(&root.bytes[..], expected, "{label} {name}");
    }
}

struct OutlineDefinition {
    cell: CellId,
    file_digest: ContentId,
    item_digest: ContentId,
}

// The fixture's outline publishes only full definitions, so the item's stable
// chain within its file suffices to select its cell. The value is the same
// whole-item hash that the CLI's outline publishes for a definition read.
fn dependency_definition<'n>(records: &'n Records<'n>, krate: usize, key: ItemKey) -> OutlineDefinition {
    let items = records.items_record(krate).expect("dependency items record");
    let row = items.item(key).expect("read dependency item").expect("dep::len item");
    let blaze_scope::resolve::At::File(at) = row.text.expect("dep::len text").at else {
        panic!("dep::len must be defined in a fixture file");
    };
    let file = items.file_surface(at).expect("read dependency file").expect("dep::len file");
    let text = records.file_text(krate, at).expect("resolve dependency file text");
    let outline = blaze_file_items::outline(text, items.edition(), blaze_own::testing::heap().alloc());
    let pieces = items.pieces(key).expect("read dependency item grain").expect("dep::len grain");
    let of = Name::file(format!("dep/{}", file.path.as_str()).as_bytes());
    let cell = blaze_drive::cells::Cell(pieces.chain.0[..16].try_into().expect("item chain prefix"));
    OutlineDefinition { cell: CellId { of, cell }, file_digest: ContentId::of("blaze v2 vfs file 1", text.as_bytes()), item_digest: outline.item(row.outline).hash }
}

// On edit only the dependency file is planned. Its outline must publish the
// edited definition before the engine can admit the caller on its saved read.
fn caller_request<'engine>(
    previous: Option<Answer<'engine>>,
    definition: OutlineDefinition,
    caller: ItemKey,
    caller_text: &str,
    mut check_caller: impl FnMut() -> (CellId, usize),
) -> Answer<'engine> {
    let heap = blaze_own::testing::heap().shared();
    let app = Name::new(Kind::BodyCheck, caller.0);
    let app_file = Name::file(b"app/src/lib.rs");
    let len = definition.cell;
    let dep_plan = [Planned { name: len.of, phase: Phase::new(Stage::Outlines, 0), source: definition.file_digest, file: len.of }];
    let app_plan = [Planned { name: app, phase: Phase::new(Stage::Bodies, 0), source: ContentId::of("caller text", caller_text.as_bytes()), file: app_file }];
    let mut files = vec![(len.of, dep_plan.as_slice())];
    let before = previous.as_ref().map(|answer| answer.cell(&len));
    if let Some(was) = before {
        assert_ne!(was, blaze_drive::key::ABSENT, "the same definition cell must exist in the cold answer");
        assert_ne!(was, definition.item_digest, "the edit must move dep::len's definition");
    } else {
        files.push((app_file, app_plan.as_slice()));
    }
    let tools = Tools([Tool(ContentId::of("hex fixture tools", b"1")); blaze_drive::key::KINDS]);
    let prior = previous.unwrap_or_else(|| Answer::new_in(heap));
    let mut engine = Engine::new_in(tools, prior, &Edit { files: &files }, Everything, Plant::None, heap).expect("plan hex caller");
    let mut buffers = [Buffer::new_in(heap)];
    loop {
        if engine.next_phase_in(Stage::Outlines, &mut |_, _| true).expect("enter outline").is_none()
            && engine.next_phase().expect("enter next phase").is_none()
        {
            break;
        }
        for task in engine.tasks() {
            let name = task.planned.name;
            let mut defines = blaze_own::Vec::new_in(heap);
            let mut reads = blaze_own::Vec::new_in(heap);
            let (out, findings) = if name == len.of {
                defines.push((len.cell, definition.item_digest));
                (definition.file_digest, 0)
            } else {
                assert_eq!(name, app);
                let (read, count) = check_caller();
                assert_eq!(read, len);
                let observed = engine.view().cell(&read);
                assert_eq!(observed, definition.item_digest, "caller must see the published dep::len definition");
                reads.push((read, observed));
                let count = u32::try_from(count).expect("fixture diagnostic count");
                (ContentId::of("caller findings", &count.to_le_bytes()), count)
            };
            buffers[0].push(Done { name, key: task.key, out, defines, reads, findings, failed: false }, Stands::ByReads);
        }
        engine.merge(&mut buffers).expect("file phase");
    }
    let finished = engine.finish().expect("finish hex request");
    let row = finished.causes.iter().find(|row| row.name == app).expect("the caller must have entered");
    if let Some(was) = before {
        assert_eq!(row.cause, Cause::ReadMoved { read: len, was, now: definition.item_digest });
    } else {
        assert_eq!(row.cause, Cause::New);
    }
    finished.answer
}

fn reader<'r>(
    frame: &'r Frame,
    arena: Alloc<'r>,
    keys: &'r [CrateItemKey],
    labels: &'r [&'r str],
    saved: &'r Saved<'_>,
) -> &'r Records<'r> {
    let log: &'r Log<'r> = frame.alloc(Log::new(labels, arena));
    frame.alloc(Records::new(keys, saved, log, frame, arena))
}

fn fixture<'engine>(provider: Provider, dep: &str, case: Case, previous: Option<Answer<'engine>>) -> Option<Answer<'engine>> {
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let edition = edition::PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let unit = |name, root, shown_from, deps| CrateInput {
        name, root, shown_from, deps, key: CrateItemKey(ContentId::of("hex fixture crate", name.as_bytes())),
        edition: syntax_of(edition), implicit: blaze_unit_graph::implicit(edition, &blaze_unit_graph::UnitKind::Lib),
        features: &[], cfg: &cfg, script: None,
    };
    let dep_index = 1;
    let (dep_name, import, extra_calls, len_path): (_, _, _, &[&str]) = match provider {
        Provider::HexLiteral => ("hex_literal", "use hex_literal as dep;", "", &["len"]),
        Provider::Alloy => ("alloy_primitives", "use alloy::primitives as dep;", ALLOY_CALLS, &["hex_literal", "len"]),
    };
    let app = format!("#![no_std]\n{import}\n{ROOT}{extra_calls}");
    let deps = [match provider {
        Provider::HexLiteral => (dep_name, dep_index),
        Provider::Alloy => ("alloy", 4),
    }];
    let facade_deps = [(dep_name, dep_index)];
    let crates = [
        unit("app", "/app/src/lib.rs", "/app/", deps.as_slice()),
        unit(dep_name, "/dep/src/lib.rs", "/dep/", &[]),
        unit("core", "/library/core/src/lib.rs", "/library/", &[]),
        unit("alloy_core", "/facade/src/lib.rs", "/facade/", &facade_deps),
        unit("alloy", "/alloy/src/lib.rs", "/alloy/", &[("alloy_core", 3)]),
    ];
    let crates = match provider {
        Provider::HexLiteral => &crates[..3],
        Provider::Alloy => &crates[..],
    };
    let sysroot = Sysroot { core: Some(CrateId(2)), ..Sysroot::default() };
    let solve_frame = Frame::new(Tag::Infer).expect("names frame");
    let texts = Files { app: &app, dep, frame: &solve_frame };
    let keep_names = blaze_own::Erased::new(&solve_frame);
    let names = resolve_crates(crates, sysroot, &texts, &Serial, keep_names.bump()).expect("resolve app and dep");
    let sources = [SourceFile { path: "src/lib.rs", text: &app }];
    {
        let mut solved = ProjectWorld::new(&names, &solve_frame, &sources, alloc).expect("world over names");
        check(&mut solved, provider, case, "names", keep_names.bump());
    }

    let labels: Vec<_> = crates.iter().map(|c| c.name).collect();
    let keys: Vec<_> = crates.iter().map(|c| c.key).collect();
    let saved = Saved::new(&names, keep_names.bump(), alloc);
    let frame = Frame::new(Tag::Infer).expect("records frame");
    let keep_records = blaze_own::Erased::new(&frame);
    let arena = keep_records.handle();
    let lang = [0, 2];
    let records = reader(&frame, arena, &keys, &labels, &saved);
    let base = Front2::from_records(records, &frame, &lang).expect("records front end");
    let len = item(records, dep_index, len_path);
    let recorder = Recorder::new(RecorderOpts { alloc: arena });
    let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &lang, reads: &recorder });
    let front = Front2 { names: tables, resolver: Paths { names: tables }, index: base.index, keys: base.keys, texts: base.texts, frame: &frame };
    let mut world = ProjectWorld::over(front, alloc);
    let request = matches!(case, Case::Valid | Case::Longer).then(|| {
        let caller = item(records, 0, &["in_body"]);
        let caller_text = records.text_of(caller).expect("caller's item text").text;
        let metadata = TableRead::Names(NameCell::Item { key: len });
        let text = TableRead::Text { key: len };
        let definition = dependency_definition(records, dep_index, len);
        caller_request(previous, definition, caller, caller_text, || {
            {
                let setup = recorder.keys().expect("fixture setup reads");
                assert!(!setup.contains(&metadata) && !setup.contains(&text));
            }
            let result = world.check(BodyRef::Free(caller), arena);
            if case == Case::Valid {
                assert!(result.is_clean(), "{result:?}");
            } else {
                assert!(result.diagnostics.iter().any(|d| d.code == "E0308"), "{result:?}");
            }
            let keys = recorder.keys().expect("complete reads for the caller alone");
            assert!(keys.contains(&metadata), "the caller must look up dep::len");
            let key = keys.iter().find_map(|read| match *read {
                TableRead::Text { key } if key == len => Some(key),
                _ => None,
            }).expect("the caller must read dep::len's definition to evaluate the array length");
            (dependency_definition(records, dep_index, key).cell, result.diagnostics.len())
        })
    });
    check(&mut world, provider, case, "records", keep_records.bump());
    assert!(records.errors().is_empty(), "record reads: {:?}", records.errors());
    request
}

#[test]
fn hex_arrays_check_and_evaluate_over_names_and_records() {
    fixture(Provider::HexLiteral, DEP, Case::Valid, None).expect("cold answer");
    println!("body, const, static and argument yield [00, ff] over names and records");
}

#[test]
fn the_reexported_decoder_uses_the_expansion_block() {
    fixture(Provider::Alloy, ALLOY_DEP, Case::Valid, None).expect("cold answer");
    println!("Alloy's reexported hex! yields [00, ff] and address!, b256! and fixed_bytes! yield the expected bytes");
}

#[test]
fn changing_only_len_changes_the_callers_array_type() {
    let edited = DEP.replace("    len\n}", "    len + 1\n}");
    assert_ne!(edited, DEP);
    let cold = fixture(Provider::HexLiteral, DEP, Case::Valid, None).expect("cold answer");
    fixture(Provider::HexLiteral, &edited, Case::Longer, Some(cold)).expect("edited answer");
    println!("the caller reads dep::len and length 3 gives E0308 in all four contexts");
}

#[test]
fn the_missing_block_input_survives_as_a_typed_cause() {
    // Force a lookup outside the expansion block. The callee and the argument
    // supplied to decode keep their original resolution.
    // The block has no `crate::STRINGS`, so lowering records the unresolved path
    // gap before an anonymous constant can be evaluated or refused.
    let planted = DEP.replace("$crate::len(STRINGS)", "$crate::len(crate::STRINGS)");
    assert_ne!(planted, DEP);
    assert!(fixture(Provider::HexLiteral, &planted, Case::MissingInput, None).is_none());
    println!("the required anonymous body's E0425 names STRINGS over names and records");
}

#[test]
fn a_refusal_identifies_the_sibling_or_callee_that_failed() {
    let sibling = DEP.replace("&[$($s.as_bytes(),)*]", "{ let _ = MISSING_BYTES; &[$($s.as_bytes(),)*] }");
    let callee = DEP.replace("let mut len = 0", "let mut len = MISSING_LENGTH");
    assert_ne!(sibling, DEP);
    assert_ne!(callee, DEP);
    assert!(fixture(Provider::HexLiteral, &sibling, Case::InvalidStrings, None).is_none());
    assert!(fixture(Provider::HexLiteral, &callee, Case::InvalidLen, None).is_none());
    println!("STRINGS names MISSING_BYTES and dep::len names MISSING_LENGTH over names and records");
}
