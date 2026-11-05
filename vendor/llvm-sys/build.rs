extern crate anyhow;
extern crate cc;
#[macro_use]
extern crate lazy_static;

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context as _;

// blaze: the pinned LLVM, the one LLVM this script links.
include!("pin.rs");

// Environment variables that can guide compilation
//
// When adding new ones, they should also be added to main() to force a
// rebuild if they are changed.
lazy_static! {
    /// If set, do not attempt to strip irrelevant options for llvm-config --cflags
    static ref ENV_NO_CLEAN_CFLAGS: String =
        format!("LLVM_SYS_{}_NO_CLEAN_CFLAGS", env!("CARGO_PKG_VERSION_MAJOR"));

    /// If set and targeting MSVC, force the debug runtime library
    static ref ENV_USE_DEBUG_MSVCRT: String =
        format!("LLVM_SYS_{}_USE_DEBUG_MSVCRT", env!("CARGO_PKG_VERSION_MAJOR"));

    /// If set, always link against libffi
    static ref ENV_FORCE_FFI: String =
        format!("LLVM_SYS_{}_FFI_WORKAROUND", env!("CARGO_PKG_VERSION_MAJOR"));
}

fn target_env_is(name: &str) -> bool {
    match env::var_os("CARGO_CFG_TARGET_ENV") {
        Some(s) => s == name,
        None => false,
    }
}

fn target_os_is(name: &str) -> bool {
    match env::var_os("CARGO_CFG_TARGET_OS") {
        Some(s) => s == name,
        None => false,
    }
}

fn target_abi_is(name: &str) -> bool {
    match env::var_os("CARGO_CFG_TARGET_ABI") {
        Some(s) => s == name,
        None => false,
    }
}

/// Invoke the specified binary as llvm-config.
fn llvm_config<I, S>(binary: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    llvm_config_ex(binary, args).expect("Surprising failure from llvm-config")
}

/// Invoke the specified binary as llvm-config.
///
/// Explicit version of the `llvm_config` function that bubbles errors
/// up.
fn llvm_config_ex<I, S>(binary: &Path, args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(binary);
    (|| {
        let Output {
            status,
            stdout,
            stderr,
        } = cmd.args(args).output()?;
        let stdout = String::from_utf8(stdout).context("stdout")?;
        let stderr = String::from_utf8(stderr).context("stderr")?;
        if status.success() {
            Ok(stdout)
        } else {
            Err(anyhow::anyhow!(
                "status={status}\nstdout={}\nstderr={}",
                stdout.trim(),
                stderr.trim()
            ))
        }
    })()
    .with_context(|| format!("{cmd:?}"))
}

/// Get the names of the dylibs required by LLVM, including the C++ standard
/// library.
fn get_system_libraries(llvm_config_path: &Path, kind: LibraryKind) -> Vec<String> {
    let link_arg = match kind {
        LibraryKind::Static => "--link-static",
        LibraryKind::Dynamic => "--link-shared",
    };

    llvm_config(llvm_config_path, ["--system-libs", link_arg])
        .split(&[' ', '\n'] as &[char])
        .filter(|s| !s.is_empty())
        .map(|flag| {
            if target_env_is("msvc") {
                // Same as --libnames, foo.lib
                flag.strip_suffix(".lib").unwrap_or_else(|| {
                    panic!(
                        "system library '{}' does not appear to be a MSVC library file",
                        flag
                    )
                })
            } else {
                if let Some(flag) = flag.strip_prefix("-l") {
                    // Linker flags style, -lfoo
                    if target_os_is("macos") {
                        // .tdb libraries are "text-based stub" files that provide lists of symbols,
                        // which refer to libraries shipped with a given system and aren't shipped
                        // as part of the corresponding SDK. They're named like the underlying
                        // library object, including the 'lib' prefix that we need to strip.
                        if let Some(flag) = flag
                            .strip_prefix("lib")
                            .and_then(|flag| flag.strip_suffix(".tbd"))
                        {
                            return flag;
                        }
                    }

                    if let Some(i) = flag.find(".so.") {
                        // On some distributions (OpenBSD, perhaps others), we get sonames
                        // like "-lz.so.7.0". Correct those by pruning the file extension
                        // and library version.
                        return &flag[..i];
                    }
                    return flag;
                }

                let maybe_lib = Path::new(flag);
                if maybe_lib.is_file() {
                    // Library on disk, likely an absolute path to a .so. We'll add its location to
                    // the library search path and specify the file as a link target.
                    println!(
                        "cargo:rustc-link-search={}",
                        maybe_lib.parent().unwrap().display()
                    );

                    // Expect a file named something like libfoo.so, or with a version libfoo.so.1.
                    // Trim everything after and including the last .so and remove the leading 'lib'
                    let soname = maybe_lib
                        .file_name()
                        .unwrap()
                        .to_str()
                        .expect("Library filename must be a valid string");

                    // Check for any valid library filename, even if it's a different kind from the
                    // one we asked for. Some configurations give us a path to a static archive,
                    // even when we're asking for shared libraries.
                    [LibraryKind::Dynamic, LibraryKind::Static]
                        .iter()
                        .filter_map(|&kind| {
                            if let Some((stem, _rest)) = soname.rsplit_once(kind.file_extension()) {
                                Some(stem.strip_prefix("lib").unwrap_or_else(|| {
                                    panic!(
                                        "system library '{}' does not have a 'lib' prefix",
                                        soname
                                    )
                                }))
                            } else {
                                None
                            }
                        })
                        .next()
                        .unwrap_or_else(|| {
                            panic!(
                                "Filename '{}' does not appear to refer to a library file",
                                soname
                            )
                        })
                } else {
                    panic!(
                        "Unable to parse result of llvm-config --system-libs: {}",
                        flag
                    )
                }
            }
        })
        .chain(get_system_libcpp())
        .map(str::to_owned)
        .collect()
}

/// Return additional linker search paths that should be used but that are not discovered
/// by other means.
///
/// In particular, this should include only directories that are known from platform-specific
/// knowledge that aren't otherwise discovered from either `llvm-config` or a linked library
/// that includes an absolute path.
fn get_system_library_dirs() -> impl IntoIterator<Item = String> {
    let mut system_library_dirs = Vec::new();

    // Add the directories provided through the environment variable.
    if let Some(dirs) = option_env!("LLVM_SYS_SYSTEM_LIBRARY_DIRS") {
        system_library_dirs.extend(dirs.split(':').map(|s| s.to_string()));
    }

    if target_os_is("openbsd") || target_os_is("freebsd") {
        system_library_dirs.push("/usr/local/lib".to_string());
    } else if target_os_is("macos") {
        if let Some(p) = homebrew_prefix(None) {
            system_library_dirs.push(format!("{}/lib", p));
        }
    } else if target_os_is("linux") && cfg!(target_feature = "crt-static") {
        // When linking statically on Linux, we need to provide the directory
        // with system-wide static libraries explicitly.
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "powerpc64",
            target_arch = "aarch64"
        ))]
        {
            system_library_dirs.push("/lib64".to_string());
            system_library_dirs.push("/usr/lib64".to_string());
            system_library_dirs.push("/usr/local/lib64".to_string());
        }
        system_library_dirs.push("/lib".to_string());
        system_library_dirs.push("/usr/lib".to_string());
        system_library_dirs.push("/usr/local/lib".to_string());
    }

    system_library_dirs
}

fn homebrew_prefix(name: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("brew");

    cmd.arg("--prefix");

    if let Some(name) = name {
        cmd.arg(name);
    }

    cmd.output()
        .ok()
        .filter(|o| !o.stdout.is_empty())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|val| val.trim().to_string())
}

/// blaze: libstdc++.a of the C++ compiler cc-rs would use (`CXX`, else `c++`).
fn static_libstdcxx() -> PathBuf {
    println!("cargo:rerun-if-env-changed=CXX");
    let cxx = env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let out = Command::new(&cxx)
        .arg("-print-file-name=libstdc++.a")
        .output()
        .unwrap_or_else(|e| panic!("{cxx} -print-file-name=libstdc++.a: {e}"));
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert!(
        out.status.success() && path.is_absolute() && path.exists(),
        "`{cxx} -print-file-name=libstdc++.a` found no libstdc++.a ({}): a static LLVM links \
         libstdc++ statically, so install the C++ compiler's static library (Debian and Ubuntu: \
         libstdc++-<version>-dev)",
        path.display()
    );
    path
}

/// Get the library that must be linked for C++, if any.
fn get_system_libcpp() -> Option<&'static str> {
    if let Some(libcpp) = option_env!("CXXSTDLIB").or(option_env!("LLVM_SYS_LIBCPP")) {
        // Use the library defined by the caller, if provided.
        // CXXSTDLIB is a de-facto standard used by cc-rs.
        Some(libcpp)
    } else if target_env_is("msvc") {
        // MSVC doesn't need an explicit one.
        None
    } else if target_os_is("macos") {
        // On OS X 10.9 and later, LLVM's libc++ is the default. On earlier
        // releases GCC's libstdc++ is default. Unfortunately we can't
        // reasonably detect which one we need (on older ones libc++ is
        // available and can be selected with -stdlib=lib++), so assume the
        // latest, at the cost of breaking the build on older OS releases
        // when LLVM was built against libstdc++.
        Some("c++")
    } else if target_os_is("freebsd") || target_os_is("openbsd") {
        Some("c++")
    } else if target_abi_is("llvm") {
        // These are `*-pc-windows-gnullvm` targets, which are based on LLVM
        // toolchain, so libc++ is the only cxx std library here
        Some("c++")
    } else {
        // Otherwise assume GCC's libstdc++.
        // This assumption is probably wrong on some platforms, but it can be
        // always overwritten through `CXXSTDLIB` variable.
        Some("stdc++")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibraryKind {
    Static,
    Dynamic,
}

impl LibraryKind {
    pub fn string(&self) -> &'static str {
        match self {
            // blaze: `-bundle`, so the archives are named to the final link rather than copied
            // into this crate's rlib (several hundred MB of LLVM per build).
            LibraryKind::Static => "static:-bundle",
            LibraryKind::Dynamic => "dylib",
        }
    }

    /// Return the file extension associated with a library kind for the target platform.
    ///
    /// The returned extension includes a leading '.', like ".so".
    pub fn file_extension(&self) -> &'static str {
        match self {
            LibraryKind::Dynamic => {
                if target_os_is("macos") {
                    ".dylib"
                } else {
                    ".so"
                }
            }
            LibraryKind::Static => ".a",
        }
    }
}

/// Get the names of libraries to link against, along with whether it is static or shared library.
fn get_link_libraries(
    llvm_config_path: &Path,
    preferences: &LinkingPreferences,
) -> (LibraryKind, Vec<String>) {
    // Using --libnames in conjunction with --libdir is particularly important
    // for MSVC when LLVM is in a path with spaces, but it is generally less of
    // a hack than parsing linker flags output from --libs and --ldflags.

    fn get_link_libraries_impl(
        llvm_config_path: &Path,
        kind: LibraryKind,
    ) -> anyhow::Result<String> {
        if target_env_is("msvc") && kind == LibraryKind::Dynamic {
            // Upstream work on support: https://github.com/llvm/llvm-project/issues/109483
            anyhow::bail!("Dynamic linking to LLVM is not currently supported on Windows");
        }

        let link_arg = match kind {
            LibraryKind::Static => "--link-static",
            LibraryKind::Dynamic => "--link-shared",
        };
        llvm_config_ex(llvm_config_path, ["--libnames", link_arg])
    }

    let LinkingPreferences {
        prefer_static,
        force,
    } = preferences;
    let one = [*prefer_static];
    let both = [*prefer_static, !*prefer_static];

    let preferences = if *force { &one[..] } else { &both[..] }
        .iter()
        .map(|is_static| {
            if *is_static {
                LibraryKind::Static
            } else {
                LibraryKind::Dynamic
            }
        });

    for kind in preferences {
        match get_link_libraries_impl(llvm_config_path, kind) {
            Ok(s) => return (kind, extract_library(&s, kind)),
            Err(err) => {
                println!(
                    "failed to get {} libraries from llvm-config: {err:?}",
                    kind.string()
                )
            }
        }
    }

    panic!("failed to get linking libraries from llvm-config",);
}

fn extract_library(s: &str, kind: LibraryKind) -> Vec<String> {
    s.split(&[' ', '\n'] as &[char])
        .filter(|s| !s.is_empty())
        .map(|name| {
            // --libnames gives library filenames. Extract only the name that
            // we need to pass to the linker.
            match kind {
                LibraryKind::Static => {
                    // Match static library
                    if let Some(name) = name
                        .strip_prefix("lib")
                        .and_then(|name| name.strip_suffix(".a"))
                    {
                        // Unix (Linux/Mac)
                        // libLLVMfoo.a
                        name
                    } else if let Some(name) = name.strip_suffix(".lib") {
                        // Windows
                        // LLVMfoo.lib
                        name
                    } else {
                        panic!("'{}' does not look like a static library name", name)
                    }
                }
                LibraryKind::Dynamic => {
                    // Match shared library
                    if let Some(name) = name
                        .strip_prefix("lib")
                        .and_then(|name| name.strip_suffix(".dylib"))
                    {
                        // Mac
                        // libLLVMfoo.dylib
                        name
                    } else if let Some(name) = name
                        .strip_prefix("lib")
                        .and_then(|name| name.strip_suffix(".so"))
                    {
                        // Linux
                        // libLLVMfoo.so
                        name
                    } else if let Some(name) = IntoIterator::into_iter([".dll", ".lib"])
                        .find_map(|suffix| name.strip_suffix(suffix))
                    {
                        // Windows
                        // LLVMfoo.{dll,lib}
                        name
                    } else {
                        panic!("'{}' does not look like a shared library name", name)
                    }
                }
            }
            .to_string()
        })
        .collect::<Vec<String>>()
}

#[derive(Debug, Clone, Copy)]
struct LinkingPreferences {
    /// Prefer static linking over dynamic linking.
    prefer_static: bool,
    /// Force the use of the preferred kind of linking.
    force: bool,
}

impl LinkingPreferences {
    fn init() -> LinkingPreferences {
        let prefer_static = cfg!(feature = "prefer-static");
        let prefer_dynamic = cfg!(feature = "prefer-dynamic");
        let force_static = cfg!(feature = "force-static");
        let force_dynamic = cfg!(feature = "force-dynamic");

        // more than one preference is an error
        if [prefer_static, prefer_dynamic, force_static, force_dynamic]
            .iter()
            .filter(|&&x| x)
            .count()
            > 1
        {
            panic!(
                "Only one of the features `prefer-static`, `prefer-dynamic`, `force-static`, \
                 `force-dynamic` can be enabled at once"
            );
        }

        // if no preference is given, default to prefer static linking
        let prefer_static = prefer_static || !(prefer_dynamic || force_static || force_dynamic);

        LinkingPreferences {
            prefer_static: force_static || prefer_static,
            force: force_static || force_dynamic,
        }
    }
}

fn get_llvm_cflags(llvm_config_path: &Path) -> String {
    let output = llvm_config(llvm_config_path, ["--cflags"]);

    // llvm-config includes cflags from its own compilation with --cflags that
    // may not be relevant to us. In particularly annoying cases, these might
    // include flags that aren't understood by the default compiler we're
    // using. Unless requested otherwise, clean CFLAGS of options that are
    // known to be possibly-harmful.
    let no_clean = env::var_os(&*ENV_NO_CLEAN_CFLAGS).is_some();
    if no_clean || target_env_is("msvc") {
        // MSVC doesn't accept -W... options, so don't try to strip them and
        // possibly strip something that should be retained. Also do nothing if
        // the user requests it.
        return output;
    }

    output
        .split(&[' ', '\n'][..])
        .filter(|word| !word.starts_with("-W"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// blaze: `llvm-config --cxxflags` for wrappers/passes.cpp, cleaned as [`get_llvm_cflags`]
/// cleans the C flags.
fn get_llvm_cxxflags(llvm_config_path: &Path) -> String {
    let output = llvm_config(llvm_config_path, ["--cxxflags"]);
    let no_clean = env::var_os(&*ENV_NO_CLEAN_CFLAGS).is_some();
    if no_clean || target_env_is("msvc") {
        return output;
    }
    output
        .split(&[' ', '\n'][..])
        .filter(|word| !word.starts_with("-W"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_llvm_debug(llvm_config_path: &Path) -> bool {
    // Has to be either Debug or Release
    llvm_config(llvm_config_path, ["--build-mode"]).contains("Debug")
}

fn main() {
    // blaze: the pinned prefix or one error (pin.rs), before any step reads or runs LLVM. No
    // search of PATH or Homebrew and no version range: a prefix that is not the pin stops here.
    let Some(llvm_config_path) = pinned_llvm_config() else {
        return;
    };

    // Behavior can be significantly affected by these vars.
    if let Ok(path) = env::var(PIN_PREFIX) {
        println!("cargo:rerun-if-changed={}", path);
    }

    println!("cargo:rerun-if-env-changed={}", &*ENV_NO_CLEAN_CFLAGS);
    println!("cargo:rerun-if-env-changed={}", &*ENV_USE_DEBUG_MSVCRT);
    println!("cargo:rerun-if-env-changed={}", &*ENV_FORCE_FFI);

    if cfg!(feature = "no-llvm-linking") && cfg!(feature = "disable-alltargets-init") {
        // exit early as we don't need to do anything and llvm-config isn't needed at all
        return;
    }

    // Build the extra wrapper functions.
    println!("cargo:rerun-if-changed=wrappers/target.c");
    if !cfg!(feature = "disable-alltargets-init") {
        std::env::set_var("CFLAGS", get_llvm_cflags(&llvm_config_path));
        cc::Build::new()
            .file("wrappers/target.c")
            .compile("targetwrappers");
    }

    // blaze: the pass builder kept per thread and target machine (wrappers/passes.cpp). LLVM's
    // own system libraries name the C++ runtime, so cc names none.
    println!("cargo:rerun-if-changed=wrappers/passes.cpp");
    let mut passes = cc::Build::new();
    passes.cpp(true).cpp_link_stdlib(None).file("wrappers/passes.cpp");
    for flag in get_llvm_cxxflags(&llvm_config_path).split_whitespace() {
        passes.flag(flag);
    }
    passes.compile("blazepasses");

    if cfg!(feature = "no-llvm-linking") {
        return;
    }

    let libdir = llvm_config(&llvm_config_path, ["--libdir"]);

    // Export information to other crates
    println!("cargo:config_path={}", llvm_config_path.display()); // will be DEP_LLVM_CONFIG_PATH
    println!("cargo:libdir={}", libdir); // DEP_LLVM_LIBDIR

    let preferences = LinkingPreferences::init();

    // Link LLVM libraries
    println!("cargo:rustc-link-search=native={}", libdir);
    for link_search_dir in get_system_library_dirs() {
        println!("cargo:rustc-link-search=native={}", link_search_dir);
    }
    // We need to take note of what kind of libraries we linked to, so that
    // we can link to the same kind of system libraries
    let (kind, libs) = get_link_libraries(&llvm_config_path, &preferences);
    for name in libs {
        println!("cargo:rustc-link-lib={}={}", kind.string(), name);
    }

    // Link system libraries
    // We get the system libraries based on the kind of LLVM libraries we link to, but we link to
    // system libs based on the target environment.
    let sys_lib_kind = if cfg!(target_feature = "crt-static") {
        LibraryKind::Static
    } else {
        LibraryKind::Dynamic
    };
    for name in get_system_libraries(&llvm_config_path, kind) {
        // blaze: a static LLVM on Linux takes its C++ runtime statically too, so a binary built
        // with it asks the system for nothing of C++.
        if name == "stdc++" && kind == LibraryKind::Static && target_os_is("linux") {
            let archive = static_libstdcxx();
            let dir = archive.parent().expect("libstdc++.a is in a directory");
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static:-bundle=stdc++");
            continue;
        }
        println!("cargo:rustc-link-lib={}={}", sys_lib_kind.string(), name);
    }

    let use_debug_msvcrt = env::var_os(&*ENV_USE_DEBUG_MSVCRT).is_some();
    if target_env_is("msvc") && (use_debug_msvcrt || is_llvm_debug(&llvm_config_path)) {
        println!("cargo:rustc-link-lib=msvcrtd");
    }

    // Link libffi if the user requested this workaround.
    // See https://bitbucket.org/tari/llvm-sys.rs/issues/12/
    let force_ffi = env::var_os(&*ENV_FORCE_FFI).is_some();
    if force_ffi {
        println!("cargo:rustc-link-lib=dylib=ffi");
    }
}
