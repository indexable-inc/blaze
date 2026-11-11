//! The spellings [`parse`] reads: each to its typed option, or to a refusal naming the
//! argument, the word, where it came from and the target.

use std::path::PathBuf;

use blaze_codegen::{NativeLib, NativeLibKind};

use super::{ArgFrom, GivenArg, Kind, LinkOption, Spelling, parse};
use crate::Undefined as Und;

const MAC: &str = "aarch64-apple-darwin";
const LINUX: &str = "x86_64-unknown-linux-gnu";

fn given(args: &[&str]) -> Vec<GivenArg> {
    args.iter().map(|it| GivenArg { arg: (*it).to_owned(), from: ArgFrom::Rustflags }).collect()
}

fn lib(name: &str, kind: NativeLibKind, verbatim: bool) -> LinkOption {
    LinkOption::Lib(NativeLib { name: name.to_owned(), kind, verbatim })
}

fn options(spelling: Spelling, kind: Kind, args: &[&str]) -> Vec<LinkOption> {
    let triple = if spelling == Spelling::Ld64 { MAC } else { LINUX };
    parse(spelling, kind, triple, &given(args)).unwrap_or_else(|errors| {
        panic!("{args:?} refused: {}", errors.iter().map(|it| it.to_string()).collect::<Vec<_>>().join("; "))
    })
}

/// The one word refused of `args`, as the error names it.
fn refused(spelling: Spelling, kind: Kind, args: &[&str]) -> String {
    let triple = if spelling == Spelling::Ld64 { MAC } else { LINUX };
    let errors = parse(spelling, kind, triple, &given(args)).expect_err("refused");
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors[0].to_string()
}

#[test]
fn spellings_to_options() {
    use Kind::{Executable as Exe, Shared};
    use LinkOption::*;
    use NativeLibKind::{Dylib, Framework};
    use Spelling::{Gnu, Ld64};
    let dir = |it: &str| Search(PathBuf::from(it));
    let table: &[(Spelling, Kind, &[&str], Vec<LinkOption>)] = &[
        // The driver's layer comes off: `-Wl,` splits on commas, `-Xlinker` takes the next.
        (Ld64, Exe, &["-Wl,-rpath,@loader_path/../lib"], vec![Rpath("@loader_path/../lib".into())]),
        (Ld64, Exe, &["-Xlinker", "-rpath", "-Xlinker", "/opt/lib"], vec![Rpath("/opt/lib".into())]),
        (Ld64, Exe, &["-rpath", "/opt/lib"], vec![Rpath("/opt/lib".into())]),
        (Gnu, Exe, &["-Wl,-rpath,$ORIGIN/../lib"], vec![Rpath("$ORIGIN/../lib".into())]),
        (Gnu, Exe, &["-Wl,-rpath=/opt/lib"], vec![Rpath("/opt/lib".into())]),
        (Gnu, Exe, &["-Wl,--rpath,/opt/lib"], vec![Rpath("/opt/lib".into())]),
        (Gnu, Exe, &["-Wl,-R,$ORIGIN"], vec![Rpath("$ORIGIN".into())]),
        // Header room.
        (Ld64, Exe, &["-Wl,-headerpad,0x1000"], vec![HeaderPad(0x1000)]),
        (Ld64, Exe, &["-Wl,-headerpad,800"], vec![HeaderPad(0x800)]),
        (Ld64, Exe, &["-headerpad_max_install_names"], vec![HeaderPadMaxInstallNames]),
        (Ld64, Shared, &["-Wl,-headerpad_max_install_names"], vec![HeaderPadMaxInstallNames]),
        // Undefined symbols: pyo3's `extension-module` gives `-undefined` and its value as two
        // arguments.
        (Ld64, Shared, &["-undefined", "dynamic_lookup"], vec![Undefined(Und::DynamicLookup)]),
        (Ld64, Exe, &["-Wl,-undefined,dynamic_lookup"], vec![Undefined(Und::DynamicLookup)]),
        (Ld64, Shared, &["-Wl,-undefined,error"], vec![Undefined(Und::Error)]),
        (Ld64, Exe, &["-Wl,-undefined,error"], vec![Already(super::reason::UNDEFINED_ERROR)]),
        (Gnu, Shared, &["-Wl,-z,defs"], vec![Undefined(Und::Error)]),
        (Gnu, Exe, &["-Wl,-z,defs"], vec![Already(super::reason::DEFS)]),
        // Libraries and where to find them.
        (Ld64, Exe, &["-framework", "Security"], vec![lib("Security", Framework, false)]),
        (Ld64, Exe, &["-Wl,-framework,Metal"], vec![lib("Metal", Framework, false)]),
        (Ld64, Exe, &["-Wl,-needed_framework,AppKit"], vec![lib("AppKit", Framework, false)]),
        (Ld64, Exe, &["-Wl,-needed-lz"], vec![lib("z", Dylib, false)]),
        (Ld64, Exe, &["-lc++"], vec![lib("c++", Dylib, false)]),
        (Ld64, Exe, &["-F/opt/Frameworks", "-L", "/opt/lib"], vec![dir("/opt/Frameworks"), dir("/opt/lib")]),
        (Gnu, Exe, &["-lstdc++", "-l", "m"], vec![lib("stdc++", Dylib, false), lib("m", Dylib, false)]),
        (Gnu, Exe, &["-l:libfoo.so.1"], vec![lib("libfoo.so.1", Dylib, true)]),
        (Gnu, Exe, &["-L/opt/lib", "-Wl,-L,/usr/lib64"], vec![dir("/opt/lib"), dir("/usr/lib64")]),
        (Gnu, Exe, &["-pthread"], vec![lib("pthread", Dylib, false)]),
        (Ld64, Exe, &["-pthread"], vec![lib("pthread", Dylib, false)]),
        // Names, exports, the stack.
        (Ld64, Shared, &["-Wl,-install_name,@rpath/libx.dylib"], vec![Name("@rpath/libx.dylib".into())]),
        (Gnu, Shared, &["-Wl,-soname,libx.so.1"], vec![Name("libx.so.1".into())]),
        (Gnu, Shared, &["-Wl,-soname=libx.so.1"], vec![Name("libx.so.1".into())]),
        (Ld64, Shared, &["-Wl,-exported_symbol,_PyInit_x"], vec![Export("PyInit_x".into())]),
        (Ld64, Exe, &["-Wl,-stack_size,0x1000000"], vec![StackSize(0x100_0000)]),
        (Gnu, Shared, &["-Wl,-z,nodelete"], vec![NoDelete]),
        (Gnu, Exe, &["-Wl,-z,nodelete"], vec![NoEffect(super::reason::NOT_UNLOADED)]),
        // What blaze's linker does anyway, joined and apart.
        (Gnu, Exe, &["-Wl,-z,now", "-Wl,-z,relro", "-Wl,-znoexecstack"], vec![
            Already(super::reason::BIND_NOW),
            Already(super::reason::RELRO),
            Already(super::reason::NOEXECSTACK),
        ]),
        (Gnu, Exe, &["-z", "now"], vec![Already(super::reason::BIND_NOW)]),
        (Gnu, Exe, &["-Wl,--gc-sections"], vec![Already(super::reason::GC)]),
        (Ld64, Exe, &["-Wl,-dead_strip"], vec![Already(super::reason::GC)]),
        (Gnu, Exe, &["-Wl,--as-needed"], vec![Already(super::reason::AS_NEEDED)]),
        (Gnu, Exe, &["-Wl,--as-needed", "-lz"], vec![Already(super::reason::AS_NEEDED), lib("z", Dylib, false)]),
        (Gnu, Exe, &["-Wl,--no-as-needed"], vec![NoEffect(super::reason::NO_AS_NEEDED)]),
        (Gnu, Exe, &["-Wl,--eh-frame-hdr"], vec![Already(super::reason::EH_FRAME_HDR)]),
        (Gnu, Exe, &["-Wl,-O1"], vec![NoEffect(super::reason::LD_O)]),
        (Gnu, Exe, &["-fuse-ld=mold"], vec![NoEffect(super::reason::LINKER)]),
        (Ld64, Exe, &["-fuse-ld=lld"], vec![NoEffect(super::reason::LINKER)]),
    ];
    for (spelling, kind, args, want) in table {
        assert_eq!(&options(*spelling, *kind, args), want, "{spelling:?} {kind:?} {args:?}");
    }
}

#[test]
fn refusals_name_the_argument_the_word_where_and_why() {
    use Kind::{Executable as Exe, Shared};
    use Spelling::{Gnu, Ld64, Msvc, WasmLd};
    let table: &[(Spelling, Kind, &[&str], &str)] = &[
        (Gnu, Exe, &["-Wl,--wrap=malloc"], "the link argument `-Wl,--wrap=malloc` (its `--wrap=malloc`)"),
        (Gnu, Exe, &["-Wl,-T,link.x"], "a layout script"),
        (Gnu, Exe, &["-Tlink.x"], "a layout script"),
        (Gnu, Exe, &["-Wl,--version-script=v.map"], "exports from an executable only"),
        (Gnu, Exe, &["-rdynamic"], "exports from an executable only"),
        (Gnu, Exe, &["-Wl,--export-dynamic"], "exports from an executable only"),
        (Gnu, Exe, &["-static"], "static"),
        (Gnu, Exe, &["-nostartfiles"], "start-file-less"),
        (Gnu, Exe, &["-Wl,-z,lazy"], "no way to undo"),
        (Gnu, Exe, &["-Wl,-z,execstack"], "no way to undo"),
        (Gnu, Exe, &["-Wl,-soname,libx.so"], "this image is an executable"),
        (Gnu, Exe, &["-Wl,--no-as-needed", "-lfoo"], "after `--no-as-needed`"),
        (Gnu, Exe, &["-Wl,-rpath"], "none follows"),
        (Gnu, Exe, &[""], "an empty argument"),
        (Gnu, Exe, &["/x/libfoo.a"], "an input file"),
        (Ld64, Exe, &["-Wl,-z,now"], "does not implement it"),
        (Ld64, Exe, &["-weak_framework", "Metal"], "weak"),
        (Ld64, Exe, &["-Wl,-weak-lfoo"], "weak"),
        (Ld64, Exe, &["-Wl,-no_compact_unwind"], "does not implement it"),
        (Ld64, Exe, &["-ObjC"], "does not implement it"),
        (Ld64, Exe, &["-Wl,-sectcreate,__TEXT,__info_plist,Info.plist"], "(its `-sectcreate`)"),
        (Ld64, Exe, &["-Wl,-undefined,suppress"], "-flat_namespace"),
        (Ld64, Exe, &["-Wl,-exported_symbols_list,syms.txt"], "exports from an executable only"),
        (Ld64, Exe, &["-Wl,-exported_symbol,_x"], "exports from an executable only"),
        (Ld64, Shared, &["-Wl,-exported_symbol,_x*"], "a pattern"),
        (Ld64, Shared, &["-Wl,-stack_size,0x1000"], "this image is a shared library"),
        (Ld64, Exe, &["-Wl,-stack_size,0x1001"], "whole 4 KiB pages"),
        (Ld64, Exe, &["-Wl,-headerpad,zz"], "not a hexadecimal size"),
        (Ld64, Exe, &["-Wl,-headerpad,0x200000"], "1 MiB"),
        (Ld64, Exe, &["-Wl,-install_name,@rpath/x"], "this image is an executable"),
        (Msvc, Exe, &["/STACK:8388608"], "no C driver"),
        (WasmLd, Exe, &["-zstack-size=65536"], "no C driver"),
    ];
    for (spelling, kind, args, want) in table {
        let error = refused(*spelling, *kind, args);
        assert!(error.contains(want), "{spelling:?} {kind:?} {args:?}: {error}");
        assert!(error.contains("from `-C link-arg`"), "{error}");
        let triple = if *spelling == Ld64 { MAC } else { LINUX };
        assert!(error.contains(&format!("linking for {triple}")), "{error}");
    }

    // A build script's argument names its package; every refused argument is named at once.
    let args = [
        GivenArg { arg: "-Wl,--wrap=a".into(), from: ArgFrom::BuildScript { package: "x 1.0.0".into() } },
        GivenArg { arg: "-Wl,-rpath,$ORIGIN".into(), from: ArgFrom::Rustflags },
        GivenArg { arg: "-Wl,--wrap=b".into(), from: ArgFrom::Rustflags },
    ];
    let errors = parse(Spelling::Gnu, Kind::Executable, LINUX, &args).unwrap_err();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].to_string().contains("the build script of `x 1.0.0`"), "{}", errors[0]);
    assert_eq!(errors[1].word, "--wrap=b");
}

/// `Rustflags::read`: what of the rustflags is the link's, as rustc reads each spelling; every
/// other flag is left to who reads it.
#[test]
fn rustflags_read_as_rustc_reads_them() {
    use super::rustflags::{CodegenOption as C, RelroLevel, Rustflags, Strip};
    let read = |flags: &[&str]| {
        let flags: Vec<String> = flags.iter().map(|it| (*it).to_owned()).collect();
        Rustflags::read(&flags)
    };
    let table: &[(&[&str], &[&str], &[&str], &[&str], &[(&str, C)])] = &[
        // (flags, link args, -l values, -L values, options as written and typed)
        (&["-C", "link-arg=-Wl,-rpath,$ORIGIN"], &["-Wl,-rpath,$ORIGIN"], &[], &[], &[]),
        (&["-Clink-args=-lz  -pthread"], &["-lz", "-pthread"], &[], &[], &[]),
        (&["--codegen=link_arg=-Wl,-z,now"], &["-Wl,-z,now"], &[], &[], &[]),
        (&["-l", "z", "-lstatic=foo", "-l", "framework=Metal"], &[], &["z", "static=foo", "framework=Metal"], &[], &[]),
        (&["-L", "native=/opt/lib", "-L/usr/local/lib"], &[], &[], &["native=/opt/lib", "/usr/local/lib"], &[]),
        (&["-C", "rpath"], &[], &[], &[], &[("-C rpath", C::Rpath(true))]),
        (&["-Crpath=no"], &[], &[], &[], &[("-C rpath=no", C::Rpath(false))]),
        (&["-C", "relro_level=full"], &[], &[], &[], &[("-C relro-level=full", C::RelroLevel(RelroLevel::Full))]),
        (&["-Clink-dead-code"], &[], &[], &[], &[("-C link-dead-code", C::LinkDeadCode(true))]),
        (&["-C", "strip=debuginfo"], &[], &[], &[], &[("-C strip=debuginfo", C::Strip(Strip::Debuginfo))]),
        (&["-C", "linker=clang"], &[], &[], &[], &[("-C linker=clang", C::Linker("clang".into()))]),
        (&["-Clinker-flavor=gnu-cc"], &[], &[], &[], &[("-C linker-flavor=gnu-cc", C::LinkerFlavor("gnu-cc".into()))]),
        (&["-C", "prefer-dynamic=off"], &[], &[], &[], &[("-C prefer-dynamic=off", C::PreferDynamic(false))]),
        (&["-C", "default-linker-libraries"], &[], &[], &[], &[("-C default-linker-libraries", C::DefaultLinkerLibraries(true))]),
        (&["-C", "link-self-contained=n"], &[], &[], &[], &[("-C link-self-contained=n", C::LinkSelfContained(false))]),
        (&["-C", "target-feature=+crt-static,+avx2"], &[], &[], &[], &[("-C target-feature=+crt-static,+avx2", C::CrtStatic(true))]),
        // Not the link's: the code generator's, cargo's, the front end's.
        (&["-C", "target-cpu=native", "-Copt-level=3", "--cfg", "x", "-Zthreads=8", "--check-cfg=cfg(y)"], &[], &[], &[], &[]),
    ];
    for (flags, args, libs, search, options) in table {
        let got = read(flags).unwrap_or_else(|why| panic!("{flags:?}: {why}"));
        let got_args: Vec<&str> = got.args.iter().map(|it| it.arg.as_str()).collect();
        assert_eq!(got_args, *args, "{flags:?}");
        assert!(got.args.iter().all(|it| it.from == ArgFrom::Rustflags), "{flags:?}");
        assert_eq!(got.libs, *libs, "{flags:?}");
        let got_search: Vec<&str> = got.search.iter().map(|it| it.to_str().unwrap()).collect();
        assert_eq!(got_search, *search, "{flags:?}");
        let got_options: Vec<(&str, C)> =
            got.options.iter().map(|it| (it.flag.as_str(), it.option.clone())).collect();
        assert_eq!(got_options, *options, "{flags:?}");
    }

    // A value rustc would not take is an error naming the flag.
    let refused: &[(&[&str], &str)] = &[
        (&["-C", "strip=everything"], "`-C strip=everything`"),
        (&["-C", "relro-level=most"], "`-C relro-level=most`"),
        (&["-C", "rpath=maybe"], "`-C rpath=maybe`"),
        (&["-C", "linker="], "no program named"),
        (&["-C", "link-self-contained=+linker"], "unstable components"),
        (&["-l"], "`-l` ends the rustflags"),
    ];
    for (flags, want) in refused {
        let why = read(flags).expect_err("refused");
        assert!(why.contains(want), "{flags:?}: {why}");
    }
}
