//! The engine's checks of what native inputs name on the line: a GNU linker script's `-lNAME`,
//! the `/DEFAULTLIB`s of the COFF objects and archive members a PE link takes, and the
//! `LC_LINKER_OPTION`s of the Mach-O objects and members a Mach-O link loads.

use std::sync::Arc;

use blaze_codegen::{Arch, Init, Static, Target};
use blaze_items::ItemKey;
use object::write::{Object as Builder, Symbol as Def, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope};

use super::{CALLER, Change, Disk, Image, Native, NativeKind, function, id, key, name, output, rel32, returns};
use crate::output::Platform;
use crate::{Debug, Format, LinkError, Output, OutputKind};

/// An object of `format` defining `defines` (each returning 1), with `directives` as its
/// `.drectve` when there are any.
fn object(format: BinaryFormat, defines: &[&str], directives: &str) -> Vec<u8> {
    let mut b = Builder::new(format, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    for it in defines {
        let at = b.append_section_data(text, &returns(1), 16);
        b.add_symbol(Def {
            name: it.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    if !directives.is_empty() {
        let drectve = b.add_section(Vec::new(), b".drectve".to_vec(), SectionKind::Linker);
        b.append_section_data(drectve, directives.as_bytes(), 1);
    }
    b.write().unwrap()
}

/// A GNU-format archive of `members` (name, bytes, the symbols it defines), with its symbol
/// index: what both ELF and COFF archives may be.
fn archive(members: &[(&str, &[u8], &[&str])]) -> Vec<u8> {
    let header = |name: &str, size: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
    let names: Vec<&str> = members.iter().flat_map(|it| it.2.iter().copied()).collect();
    let index_len = 4 + 4 * names.len() + names.iter().map(|it| it.len() + 1).sum::<usize>();
    let mut offsets = Vec::new();
    let mut at = 8 + 60 + index_len.next_multiple_of(2);
    for (_, bytes, defines) in members {
        offsets.extend(defines.iter().map(|_| at as u32));
        at += 60 + bytes.len().next_multiple_of(2);
    }
    let mut out = b"!<arch>\n".to_vec();
    out.extend_from_slice(header("/", index_len).as_bytes());
    out.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for offset in offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    for it in &names {
        out.extend_from_slice(it.as_bytes());
        out.push(0);
    }
    if out.len() % 2 != 0 {
        out.push(b'\n');
    }
    for (member, bytes, _) in members {
        out.extend_from_slice(header(&format!("{member}/"), bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
        if out.len() % 2 != 0 {
            out.push(b'\n');
        }
    }
    out
}

/// An x86-64 ELF shared object with no sections: a library that exports nothing.
fn empty_shared() -> Vec<u8> {
    let mut out = vec![0u8; 64];
    out[..4].copy_from_slice(b"\x7fELF");
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    out[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out
}

const SHARED: NativeKind = NativeKind::Shared { as_needed: true };
const LAZY: NativeKind = NativeKind::Archive { whole: false };

/// Stages `_start` (calling `calls`, when given) with `natives`, each `(file, bytes, kind)` in
/// line order.
fn stage(output: Output, entry: &str, calls: Option<&str>, natives: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    let relocs = calls.map(|it| vec![rel32(1, Target::Extern(it.into()))]).unwrap_or_default();
    let code = if calls.is_some() { CALLER } else { returns(0) };
    let start = function(1, &code, relocs);
    let mut image = Image::new(name(super::TO), output).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![(entry.into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: natives
            .iter()
            .enumerate()
            .map(|(at, (file, bytes, kind))| Native { id: id(100 + at as u8), name: file, bytes, kind: *kind, order: at as u32 })
            .collect(),
        ..Change::default()
    };
    image.stage(change).map(drop)
}

fn elf(natives: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    stage(output(false), "_start", None, natives)
}

const DEBIAN_GCC_S: &[u8] = b"/* GNU ld script\n   Use the shared library, but some functions are only in\n   the static library.  */\nGROUP ( libgcc_s.so.1 -lgcc )\n";

#[test]
fn debians_libgcc_s_script_finds_libgcc_on_the_line() {
    let script: Arc<[u8]> = DEBIAN_GCC_S.into();
    let so: Arc<[u8]> = empty_shared().into();
    let member = object(BinaryFormat::Elf, &["__popcountdi2"], "");
    let libgcc: Arc<[u8]> = archive(&[("_popcountsi2.o", &member, &["__popcountdi2"])]).into();
    let gcc_s = ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc_s.so", &script, SHARED);
    let so1 = ("/lib/x86_64-linux-gnu/libgcc_s.so.1", &so, SHARED);
    elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc.a", &libgcc, LAZY)]).unwrap();
    elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc.so", &so, SHARED)]).expect("a libgcc.so is -lgcc too");

    let error = elf(&[gcc_s, so1]).unwrap_err();
    for said in ["-lgcc", "libgcc_s.so", "libgcc.so or libgcc.a"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    let error = elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc_eh.a", &libgcc, LAZY)]).unwrap_err();
    assert!(error.what.contains("-lgcc"), "libgcc_eh.a is not -lgcc: {}", error.what);
}

#[test]
fn a_scripts_verbatim_library_is_the_file_it_names() {
    let script: Arc<[u8]> = b"INPUT ( -l:libfoo.so.2 )".as_slice().into();
    let so: Arc<[u8]> = empty_shared().into();
    elf(&[("/usr/lib/libfoo.so", &script, SHARED), ("/opt/lib/libfoo.so.2", &so, SHARED)]).unwrap();
    let error = elf(&[("/usr/lib/libfoo.so", &script, SHARED), ("/opt/lib/libfoo.so.3", &so, SHARED)]).unwrap_err();
    assert!(error.what.contains("-l:libfoo.so.2") && error.what.contains("has no libfoo.so.2"), "{}", error.what);
}

#[test]
fn glibcs_libc_script_is_checked_as_before() {
    let script: Arc<[u8]> = b"/* GNU ld script */\nOUTPUT_FORMAT(elf64-x86-64)\nGROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a  AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )\n"
        .as_slice()
        .into();
    let so: Arc<[u8]> = empty_shared().into();
    let member = object(BinaryFormat::Elf, &["atexit"], "");
    let nonshared: Arc<[u8]> = archive(&[("atexit.oS", &member, &["atexit"])]).into();
    let script_at = "/usr/lib/x86_64-linux-gnu/libc.so";
    let all = [
        (script_at, &script, SHARED),
        ("/lib/x86_64-linux-gnu/libc.so.6", &so, SHARED),
        ("/usr/lib/x86_64-linux-gnu/libc_nonshared.a", &nonshared, LAZY),
        ("/lib64/ld-linux-x86-64.so.2", &so, SHARED),
    ];
    elf(&all).unwrap();
    let error = elf(&all[..3]).unwrap_err();
    assert!(error.what.contains("names /lib64/ld-linux-x86-64.so.2, which the link line does not have"), "{}", error.what);
}

fn windows() -> Output {
    Output {
        format: Format::Pe,
        arch: Arch::X86_64,
        kind: OutputKind::Executable { entry: "mainCRTStartup".into() },
        interpreter: None,
        platform: None,
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 1 << 32,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        rpaths: Vec::new(),
        triple: "x86_64-pc-windows-msvc".into(),
        stack: Some(1 << 20),
    }
}

/// `mainCRTStartup` calling `crt_helper`, which `msvcrt.lib`'s `crt.obj` defines and whose
/// `.drectve` is `directives`, with `more` after it on the line.
fn pe(directives: &str, more: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    let member = object(BinaryFormat::Coff, &["crt_helper"], directives);
    let unused = object(BinaryFormat::Coff, &["crt_unused"], "/DEFAULTLIB:never_taken");
    let msvcrt: Arc<[u8]> = archive(&[("crt.obj", &member, &["crt_helper"]), ("unused.obj", &unused, &["crt_unused"])]).into();
    let mut natives = vec![("C:\\VC\\lib\\x64\\msvcrt.lib", &msvcrt, LAZY)];
    natives.extend_from_slice(more);
    stage(windows(), "mainCRTStartup", Some("crt_helper"), &natives)
}

#[test]
fn a_members_default_library_is_taken_from_the_line() {
    let empty: Arc<[u8]> = b"!<arch>\n".as_slice().into();
    let vcruntime = ("C:\\VC\\lib\\x64\\VCRUNTIME.lib", &empty, LAZY);
    let ucrt = ("C:\\Kits\\ucrt\\x64\\ucrt.lib", &empty, LAZY);
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:\"ucrt.lib\"", &[vcruntime, ucrt]).unwrap();

    let error = pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:\"ucrt.lib\"", &[vcruntime]).unwrap_err();
    for said in ["ucrt.lib", "(/DEFAULTLIB)", "msvcrt.lib(crt.obj)"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    assert!(!error.what.contains("never_taken"), "a member not taken asks for nothing: {}", error.what);
}

#[test]
fn nodefaultlib_takes_a_default_library_out() {
    let empty: Arc<[u8]> = b"!<arch>\n".as_slice().into();
    let vcruntime = ("C:\\VC\\lib\\x64\\vcruntime.lib", &empty, LAZY);
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt /NODEFAULTLIB:LIBCMT.lib", &[vcruntime]).unwrap();
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt /NODEFAULTLIB", &[]).unwrap();

    let other: Arc<[u8]> = object(BinaryFormat::Coff, &["other_fn"], "/NODEFAULTLIB:libcmt").into();
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt", &[vcruntime, ("other.obj", &other, NativeKind::Object)])
        .expect("another loaded object's /NODEFAULTLIB takes it out of the whole link");
    let error = pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt", &[vcruntime]).unwrap_err();
    assert!(error.what.contains("libcmt.lib") && error.what.contains("msvcrt.lib(crt.obj)"), "{}", error.what);
}

/// A COFF object laying out the TLS template as tlssup.obj does: `_tls_start` in `.tls`,
/// `_tls_end` in `.tls$ZZZ`.
fn tlssup() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    for (section, name) in [(".tls", "_tls_start"), (".tls$ZZZ", "_tls_end")] {
        // tlssup.obj's `.tls` sections are plain initialized data: COFF has no TLS section kind.
        let id = b.add_section(Vec::new(), section.as_bytes().to_vec(), SectionKind::Data);
        let at = b.append_section_data(id, &[0; 8], 8);
        b.add_symbol(Def {
            name: name.as_bytes().to_vec(),
            value: at,
            size: 8,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(id),
            flags: SymbolFlags::None,
        });
    }
    b.write().unwrap()
}

fn thread_local(n: u8) -> Static {
    Static {
        item: ItemKey(id(n)),
        instance: id(n + 100),
        align: 8,
        init: Init::Bytes { bytes: vec![n; 8], relocs: Vec::new().into() },
        data: Vec::new(),
        mutable: false,
        thread_local: true,
        section: None,
        imports: Vec::new(),
    }
}

/// An incremental PE link that adds a thread-local lays it out before `.tls$ZZZ`, as a full
/// link would: the CRT's `_tls_used` ends the template at `_tls_end`, so one grown at the
/// region's end would be outside every thread's block.
#[test]
fn a_new_thread_local_in_an_incremental_pe_link_is_before_the_tls_end() {
    // `lea rax, [rip + _tls_start]; lea rcx, [rip + _tls_end]; ret`
    let code = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0x48, 0x8d, 0x0d, 0, 0, 0, 0, 0xc3];
    let relocs = vec![rel32(3, Target::Extern("_tls_start".into())), rel32(10, Target::Extern("_tls_end".into()))];
    let start = function(1, &code, relocs);
    let tlssup: Arc<[u8]> = tlssup().into();
    let (one, two) = (thread_local(10), thread_local(11));
    let mut image = Image::new(name(super::TO), Output { incremental: true, ..windows() }).unwrap();
    let mut disk = Disk::default();
    let check = |image: &Image, statics: usize, what: &str| {
        let state = &image.state;
        let tls: Vec<_> = state.live_pieces().filter(|it| *it.class() == crate::model::Class::Tdata).collect();
        let native = |it: &crate::state::PieceRef<'_>| matches!(it.what(), crate::state::What::Native { .. });
        let natives: Vec<u64> = tls.iter().filter(|it| native(it)).map(|it| it.addr).collect();
        let ours: Vec<(u64, u64)> = tls.iter().filter(|it| !native(it)).map(|it| (it.addr, it.size)).collect();
        assert_eq!((natives.len(), ours.len()), (2, statics), "{what}: {natives:x?} {ours:x?}");
        let (first, end) = (natives.iter().min().unwrap(), natives.iter().max().unwrap());
        assert!(ours.iter().all(|(at, size)| at > first && at + size <= *end), "{what}: {first:#x} {ours:x?} {end:#x}");
    };
    let change = Change {
        added: vec![&start],
        statics: vec![&one],
        names: vec![("mainCRTStartup".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: vec![Native { id: id(100), name: "tlssup.obj", bytes: &tlssup, kind: NativeKind::Object, order: 0 }],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    check(&image, 1, "the first link");
    disk.link(&mut image, Change { statics: vec![&two], ..Change::default() }).unwrap();
    check(&image, 2, "a thread-local added");
}

fn macos() -> Output {
    Output {
        format: Format::MachO,
        kind: OutputKind::Executable { entry: "main".into() },
        interpreter: None,
        platform: Some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000f_0000 }),
        triple: "x86_64-apple-darwin".into(),
        ..output(false)
    }
}

/// A `.tbd` of the x86-64 macOS library `install` exporting `symbol`.
fn tbd(install: &str, symbol: &str) -> Arc<[u8]> {
    format!(
        "--- !tapi-tbd\ntbd-version: 4\ntargets: [ x86_64-macos ]\ninstall-name: {install}\n\
         exports:\n  - targets: [ x86_64-macos ]\n    symbols: [ {symbol} ]\n...\n"
    )
    .into_bytes()
    .into()
}

/// An x86-64 Mach-O object defining `defines` whose `LC_LINKER_OPTION`s are `options`.
fn autolinking(defines: &[&'static str], options: &[&[&'static str]]) -> Vec<u8> {
    crate::macho::linking_object(object::macho::CPU_TYPE_X86_64, defines, options)
}

/// A macOS library on the line, as `locate` puts one there.
const MACHO_SHARED: NativeKind = NativeKind::Shared { as_needed: false };
const SDK_LIBZ: &str = "/sdk/usr/lib/libz.tbd";
const SDK_SECURITY: &str = "/sdk/System/Library/Frameworks/Security.framework/Security.tbd";

/// `main` calling `helper`, which `libsec.a`'s `sec.o` defines with `options`, after `glue.o`
/// (always loaded) asking for `-lz`, with `more` after them on the line.
fn macho(options: &[&[&'static str]], more: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    let member = autolinking(&["_helper"], options);
    let unused = autolinking(&["_unused"], &[&["-lnever_taken"]]);
    let libsec: Arc<[u8]> = archive(&[("sec.o", &member, &["_helper"]), ("unused.o", &unused, &["_unused"])]).into();
    let glue: Arc<[u8]> = autolinking(&["_glue"], &[&["-lz"]]).into();
    let mut natives = vec![("/x/glue.o", &glue, NativeKind::Object), ("/x/libsec.a", &libsec, LAZY)];
    natives.extend_from_slice(more);
    stage(macos(), "main", Some("helper"), &natives)
}

#[test]
fn an_objects_and_a_members_linker_options_are_taken_from_the_line() {
    let libz = tbd("/usr/lib/libz.1.dylib", "_deflate");
    let security = tbd("/System/Library/Frameworks/Security.framework/Versions/A/Security", "_SecRandomCopyBytes");
    let z = (SDK_LIBZ, &libz, MACHO_SHARED);
    let sec = (SDK_SECURITY, &security, MACHO_SHARED);
    macho(&[&["-framework", "Security"]], &[z, sec]).unwrap();
    macho(&[&["-framework", "Security"], &["-lz"]], &[z, sec]).expect("one library asked twice is checked once");
    let libz_a: Arc<[u8]> = archive(&[]).into();
    macho(&[], &[("/opt/lib/libz.a", &libz_a, LAZY)]).expect("a libz.a is -lz too");

    let error = macho(&[&["-framework", "Security"]], &[z]).unwrap_err();
    for said in ["-framework Security", "(LC_LINKER_OPTION)", "libsec.a(sec.o)", "does not have"] {
        assert!(error.what.contains(said), "{said}: {}", error.what);
    }
    assert!(!error.what.contains("never_taken"), "a member not taken asks for nothing: {}", error.what);

    let error = macho(&[], &[sec]).unwrap_err();
    assert!(error.what.contains("/x/glue.o asks for -lz (LC_LINKER_OPTION)"), "an object's own: {}", error.what);
    let wrong = tbd("/usr/lib/libzz.dylib", "_zz");
    let error = macho(&[], &[("/sdk/usr/lib/libzz.tbd", &wrong, MACHO_SHARED), sec]).unwrap_err();
    assert!(error.what.contains("-lz"), "libzz.tbd is not -lz: {}", error.what);
    let error = macho(&[&["-framework", "Security"]], &[z, ("/sdk/Security.tbd", &security, MACHO_SHARED)]).unwrap_err();
    assert!(error.what.contains("-framework Security"), "a framework is its NAME.framework directory's: {}", error.what);
}

#[test]
fn a_loaded_members_unknown_or_weak_linker_option_is_an_error() {
    let libz = tbd("/usr/lib/libz.1.dylib", "_deflate");
    let z = (SDK_LIBZ, &libz, MACHO_SHARED);
    let error = macho(&[&["-needed_framework", "Security"]], &[z]).unwrap_err();
    assert!(error.what.contains("libsec.a(sec.o)") && error.what.contains("`-needed_framework Security`"), "{}", error.what);
    let error = macho(&[&["-weak-lsec"]], &[z]).unwrap_err();
    assert!(error.what.contains("libsec.a(sec.o)") && error.what.contains("no weak library"), "{}", error.what);
}
