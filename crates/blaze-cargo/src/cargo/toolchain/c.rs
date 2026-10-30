//! blaze's own C toolchain, which the C and C++ that build scripts compile is compiled with in
//! place of the machine's (docs/c-toolchain.md). For the wasm32 WASI targets it is wasi-sdk:
//! its clang, archiver and `share/wasi-sysroot`, at the release the pinned nightly builds its
//! wasm32 wasi-libc with, so that a C file is compiled against the headers of the `libc.a` the
//! module links. Each host's archive is checked against the sha256 `c_pins.txt` has for it
//! (`harness/c_toolchain_pins.py` writes that) and laid out once in `~/.blaze/c`.
//!
//! What a build script is given of it is the target's own variables ([`for_script`]), which
//! cc-rs and cmake read before `TARGET_CC` and `CC`, so the host's compiler and flags stay the
//! host's. `BLAZE_C_TOOLCHAIN=given` is the one way out.

use std::{
    io::{self, BufReader},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::Context as _;

use super::own::{self, NotPublished, Pin, Source};

/// The wasi-sdk release, and the sha256 and size of its archive, by host.
const PINS: &str = include_str!("c_pins.txt");

/// The setting that leaves the C toolchain to the environment: `given`. Unset, or `own`, is
/// blaze's.
pub const VAR: &str = "BLAZE_C_TOOLCHAIN";

/// What names a mirror of the archives, or a directory with them, for a machine with no network.
pub const DIST: &str = "BLAZE_C_DIST";

/// The variables blaze sets for a target, each as `<name>_<target>`; cc-rs also reads them as
/// `<name>_<target with dashes>`, which the environment may not set either.
const TOOLS: [&str; 6] = ["CC", "CXX", "AR", "RANLIB", "CFLAGS", "CXXFLAGS"];

/// The variable cc-rs reads for a WASI target's sysroot, whatever the target.
const WASI_SYSROOT: &str = "WASI_SYSROOT";

/// One host's wasi-sdk.
pub struct Sdk {
    version: &'static str,
    address: &'static str,
    host: &'static str,
    file: &'static str,
    pin: Pin,
}

impl Sdk {
    /// The wasi-sdk of this host; `None` on a host blaze pins none for.
    pub fn ours() -> Option<Sdk> {
        Sdk::read(PINS, own::host()?)
    }

    fn read(pins: &'static str, host: &'static str) -> Option<Sdk> {
        let mut release = None;
        let mut archive = None;
        for line in pins.lines().filter(|it| !it.starts_with('#')) {
            match line.split_whitespace().collect::<Vec<_>>()[..] {
                ["wasi-sdk", version, address] => release = Some((version, address)),
                [of, "wasi-sdk", file, sha256, bytes] if of == host => {
                    archive = Some((file, Pin { sha256, bytes: bytes.parse().ok()? }))
                }
                _ => {}
            }
        }
        let ((version, address), (file, pin)) = (release?, archive?);
        Some(Sdk { version, address, host, file, pin })
    }

    /// Where blaze keeps it: named for the release, the host and its archive's sha256, so that
    /// what a build script is given names the bytes it runs.
    pub fn dir(&self) -> Option<PathBuf> {
        let name = format!("wasi-sdk-{}-{}-{}", self.version, self.host, &self.pin.sha256[..16]);
        Some(own::home()?.join("c").join(name))
    }
}

/// Whether blaze gives the C of a build script for `target` its own toolchain. The other
/// targets are still the environment's, until their steps of docs/c-toolchain.md land.
pub fn pinned_for(target: &str) -> bool {
    target.starts_with("wasm32-wasip")
}

/// What a build script is given of the C toolchain: the variables, and the directory they name,
/// which the sandbox lets it read.
#[derive(Debug, PartialEq)]
pub struct Given {
    pub env: Vec<(String, String)>,
    pub dir: PathBuf,
}

/// What a build script run with `env`, cargo's for it, is given of blaze's C toolchain: `None`
/// for a target blaze does not pin one for, or with `BLAZE_C_TOOLCHAIN=given`. `Err` names a
/// variable of the target's the environment already sets, a setting it does not know, or a
/// toolchain it could not get.
pub fn for_script(env: &[(String, String)]) -> anyhow::Result<Option<Given>> {
    let Some(target) = wanted(env)? else { return Ok(None) };
    let sdk = Sdk::ours().with_context(|| {
        format!(
            "blaze compiles the C of a build script for {target} with its own C toolchain, and \
             pins none for this host ({}-{}); `{VAR}=given` compiles it with the environment's",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    })?;
    let dir = fetch(&sdk)?;
    Ok(Some(given(target, &dir)))
}

/// The target of a run with `env` that blaze gives its C toolchain to, after the checks of
/// [`for_script`].
fn wanted(env: &[(String, String)]) -> anyhow::Result<Option<&str>> {
    let var = |name: &str| env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    let Some(target) = var("TARGET").filter(|it| pinned_for(it)) else { return Ok(None) };
    match var(VAR).unwrap_or_default() {
        "given" => return Ok(None),
        "" | "own" => {}
        other => anyhow::bail!("{VAR} is `{other}`; it is `own` (the default) or `given`"),
    }
    let underscored = target.replace('-', "_");
    let names = TOOLS
        .iter()
        .flat_map(|tool| [format!("{tool}_{target}"), format!("{tool}_{underscored}")])
        .chain([WASI_SYSROOT.to_owned()]);
    if let Some(set) = names.into_iter().find(|it| var(it).is_some()) {
        anyhow::bail!(
            "`{set}` is set, and blaze compiles the C of build scripts for {target} with its own \
             C toolchain (docs/c-toolchain.md): unset it, or set `{VAR}=given` to compile with \
             the environment's"
        );
    }
    Ok(Some(target))
}

/// The variables of `target` that name the wasi-sdk at `dir`.
fn given(target: &str, dir: &Path) -> Given {
    let exe = std::env::consts::EXE_SUFFIX;
    let bin = |name: &str| dir.join("bin").join(format!("{name}{exe}")).display().to_string();
    let sysroot = dir.join("share").join("wasi-sysroot").display().to_string();
    let flags = format!("--sysroot={sysroot}");
    let target = target.replace('-', "_");
    let env = [
        ("CC", bin("clang")),
        ("CXX", bin("clang++")),
        ("AR", bin("llvm-ar")),
        ("RANLIB", bin("llvm-ranlib")),
        ("CFLAGS", flags.clone()),
        ("CXXFLAGS", flags),
    ];
    let mut env: Vec<(String, String)> =
        env.into_iter().map(|(name, value)| (format!("{name}_{target}"), value)).collect();
    env.push((WASI_SYSROOT.to_owned(), sysroot));
    Given { env, dir: dir.to_owned() }
}

/// Whether `dir` has a whole wasi-sdk: what is moved into place is whole, so this tells one from
/// a directory someone else left there.
fn whole(dir: &Path) -> bool {
    let clang = format!("bin/clang{}", std::env::consts::EXE_SUFFIX);
    dir.join(clang).is_file() && dir.join("share/wasi-sysroot/include").is_dir()
}

/// `sdk` laid out in blaze's directory, downloaded there unless it is. Processes that ask at
/// once download it once: the others wait for the first.
pub fn fetch(sdk: &Sdk) -> anyhow::Result<PathBuf> {
    let failed = |e: &dyn std::fmt::Display| {
        anyhow::anyhow!(
            "blaze compiles the C of build scripts for wasm32 with wasi-sdk {}, which it \
             downloads once, and getting {} failed: {e}. Running the command again tries again; \
             `{DIST}` names a mirror or a directory that has the file",
            sdk.version,
            sdk.file
        )
    };
    let dir = sdk.dir().ok_or_else(|| {
        failed(&"there is no home directory to keep it in, and `BLAZE_HOME` names none")
    })?;
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(failed(&"blaze's directory has no place for it"));
    };
    let name = name.to_string_lossy().into_owned();
    std::fs::create_dir_all(parent).map_err(|e| failed(&e))?;
    let lock = std::fs::File::create(parent.join(format!(".{name}.lock"))).map_err(|e| failed(&e))?;
    lock.lock().map_err(|e| failed(&e))?;
    if whole(&dir) {
        return Ok(dir);
    }
    eprintln!(
        "Downloading wasi-sdk {} for the C of build scripts for wasm32 ({} MB, once)...",
        sdk.version,
        (sdk.pin.bytes + 500_000) / 1_000_000
    );
    let start = Instant::now();
    let part = parent.join(format!(".{name}.download"));
    let staging = parent.join(format!(".{name}.unpacking"));
    let fetched = (|| {
        let source = Source::at(DIST, sdk.address)?;
        let runtime = crate::remote::runtime()?;
        runtime.block_on(own::download(&source, sdk.file, &part, sdk.pin, None))?;
        drop(runtime);
        own::remove_dir(&staging)?;
        std::fs::create_dir_all(&staging)?;
        unpack(&part, &staging)?;
        let top = sdk.file.strip_suffix(".tar.gz").unwrap_or(sdk.file);
        let unpacked = staging.join(top);
        anyhow::ensure!(whole(&unpacked), "{} has no {top}/bin/clang and sysroot", sdk.file);
        own::remove_dir(&dir)?;
        std::fs::rename(&unpacked, &dir)?;
        own::remove_dir(&staging)?;
        std::fs::remove_file(&part)?;
        anyhow::Ok(())
    })();
    if let Err(e) = fetched {
        _ = own::remove_dir(&staging);
        if e.is::<NotPublished>() {
            return Err(failed(&format!(
                "the download is not the file wasi-sdk published (its sha256 is not {}), so blaze \
                 removed it",
                sdk.pin.sha256
            )));
        }
        return Err(failed(&format!("{e:#}")));
    }
    eprintln!("Downloaded wasi-sdk {} in {:.1?}.", sdk.version, start.elapsed());
    for old in superseded(sdk, parent, &name) {
        if let Err(e) = own::remove_dir(&old) {
            eprintln!("blaze could not remove {}, which it no longer builds with: {e}", old.display());
        }
    }
    Ok(dir)
}

/// The other wasi-sdks of this host in `parent`: an older pin's, which no build uses again.
fn superseded(sdk: &Sdk, parent: &Path, ours: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else { return Vec::new() };
    let host = format!("-{}-", sdk.host);
    entries
        .flatten()
        .filter(|it| {
            let name = it.file_name().to_string_lossy().into_owned();
            name != ours && name.starts_with("wasi-sdk-") && name.contains(&host) && whole(&it.path())
        })
        .map(|it| it.path())
        .collect()
}

/// Unpacks the `.tar.gz` at `archive` into `into`. tar refuses an entry whose path leaves it.
fn unpack(archive: &Path, into: &Path) -> io::Result<()> {
    let file = BufReader::with_capacity(1 << 20, std::fs::File::open(archive)?);
    tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
    }

    #[test]
    fn every_host_blaze_ships_has_a_pinned_wasi_sdk() {
        for host in [
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        ] {
            let sdk = Sdk::read(PINS, host).unwrap_or_else(|| panic!("no wasi-sdk for {host}"));
            assert!(sdk.file.starts_with(&format!("wasi-sdk-{}-", sdk.version)), "{}", sdk.file);
            assert_eq!(sdk.pin.sha256.len(), 64);
            assert!(sdk.address.starts_with("https://github.com/WebAssembly/wasi-sdk/"));
        }
        assert!(Sdk::read(PINS, "riscv64gc-unknown-linux-gnu").is_none());
    }

    #[test]
    fn the_wasi_targets_get_the_toolchain_and_the_others_do_not_yet() {
        for target in ["wasm32-wasip1", "wasm32-wasip1-threads", "wasm32-wasip2", "wasm32-wasip3"] {
            assert_eq!(wanted(&env(&[("TARGET", target)])).unwrap(), Some(target));
        }
        for target in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "wasm32-unknown-unknown"] {
            assert_eq!(wanted(&env(&[("TARGET", target)])).unwrap(), None);
        }
        assert_eq!(wanted(&env(&[])).unwrap(), None);
    }

    #[test]
    fn given_leaves_the_c_toolchain_to_the_environment() {
        let given = env(&[("TARGET", "wasm32-wasip1"), (VAR, "given"), ("CC_wasm32_wasip1", "gcc")]);
        assert_eq!(wanted(&given).unwrap(), None);
        let own = env(&[("TARGET", "wasm32-wasip1"), (VAR, "own")]);
        assert_eq!(wanted(&own).unwrap(), Some("wasm32-wasip1"));
        let wrong = wanted(&env(&[("TARGET", "wasm32-wasip1"), (VAR, "system")])).unwrap_err();
        assert!(wrong.to_string().contains("`own` (the default) or `given`"), "{wrong}");
    }

    #[test]
    fn a_variable_of_the_target_the_environment_sets_stops_the_build_naming_it() {
        for set in ["CC_wasm32_wasip1", "CC_wasm32-wasip1", "CFLAGS_wasm32_wasip1", "WASI_SYSROOT"] {
            let why = wanted(&env(&[("TARGET", "wasm32-wasip1"), (set, "x")])).unwrap_err();
            let why = why.to_string();
            assert!(why.contains(&format!("`{set}` is set")) && why.contains(VAR), "{why}");
        }
        // The host's compiler and flags are not the target's, and stay as they are.
        let host = env(&[("TARGET", "wasm32-wasip1"), ("CC", "gcc"), ("CFLAGS", "-O2"), ("TARGET_CC", "cc")]);
        assert_eq!(wanted(&host).unwrap(), Some("wasm32-wasip1"));
    }

    #[test]
    fn the_target_s_variables_name_the_sdk_s_compiler_and_sysroot() {
        let dir = Path::new("/home/u/.blaze/c/wasi-sdk-34.0-x86_64-unknown-linux-gnu-b761e3a0721dbae9");
        let got = given("wasm32-wasip1-threads", dir);
        let exe = std::env::consts::EXE_SUFFIX;
        let sysroot = dir.join("share").join("wasi-sysroot").display().to_string();
        let var = |name: &str| got.env.iter().find(|it| it.0 == name).map(|it| it.1.clone());
        assert_eq!(
            var("CC_wasm32_wasip1_threads"),
            Some(dir.join("bin").join(format!("clang{exe}")).display().to_string())
        );
        assert_eq!(
            var("AR_wasm32_wasip1_threads"),
            Some(dir.join("bin").join(format!("llvm-ar{exe}")).display().to_string())
        );
        assert_eq!(var("CFLAGS_wasm32_wasip1_threads"), Some(format!("--sysroot={sysroot}")));
        assert_eq!(var("CXXFLAGS_wasm32_wasip1_threads"), Some(format!("--sysroot={sysroot}")));
        assert_eq!(var(WASI_SYSROOT), Some(sysroot));
        assert_eq!(var("CC"), None);
        assert_eq!(got.dir, dir);
    }
}
