//! The DuckDB that reads the profile history (`blaze slow`, `blaze explain <view>`, `blaze debug
//! sql`, `blaze debug time`), found without Nix: an installed blaze fetches the DuckDB it pins
//! on first use, as it fetches its toolchain and LLVM.
//!
//! The order ([`locate`]), each step an answer or an error, never a quiet next:
//!
//! 1. [`observe::DUCKDB`] set: that file (an absolute path, checked by [`observe::duckdb`]); set
//!    wrong is an error naming it, and nothing else is looked at.
//! 2. The pinned DuckDB already laid out under the state directory ([`observe::Pin::laid_out`]).
//! 3. The pin fetched: its `.gz` through the daemon's store, which checks its SHA-256 before a
//!    byte is used ([`Request::FetchUrl`], blaze-fetch), unpacked beside where it goes and
//!    renamed into place, so the path holds the whole program or is absent.
//!
//! A host the pin has no DuckDB for, a fetch that fails or a digest that differs is an error
//! naming the pin (its version, URL and SHA-256) and the variable that names another. Never a
//! `duckdb` found on `PATH`: an unpinned one reads the views differently.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use blaze_api::{Request, ResponseRef};

use super::{daemon::Start, observe::ask};
use crate::serve::observe::{self, Pin};

/// The most bytes of the pin's archive: each is 17 to 36 MB.
const MOST_BYTES: u64 = 64 << 20;

/// The DuckDB to run: [`observe::DUCKDB`]'s when `named` (the variable's value) is set, else the
/// host's `pin` laid out under `state`, fetched with `fetch` (the archive's path, given the pin
/// and a scratch directory) when it is not there yet. See the module for the order.
pub(super) fn locate(
    named: Option<OsString>,
    state: &Path,
    pin: Option<&Pin>,
    fetch: impl FnOnce(&Pin, &Path) -> Result<PathBuf, String>,
) -> Result<PathBuf, String> {
    if let Some(named) = observe::duckdb(named)? {
        return Ok(named);
    }
    let Some(pin) = pin else {
        // alloc: error path
        return Err(format!(
            "blaze pins no DuckDB {} for this host ({}-{}): set {} to the absolute path of a DuckDB \
             {} CLI",
            observe::DUCKDB_VERSION,
            std::env::consts::ARCH,
            std::env::consts::OS,
            observe::DUCKDB,
            observe::DUCKDB_VERSION
        ));
    };
    let to = pin.laid_out(state);
    if to.is_file() {
        return Ok(to);
    }
    // alloc: error path
    let failed = |what: String| {
        format!(
            "the pinned DuckDB {} ({}, sha256:{}) could not be laid out at {}: {what}; {} names \
             another DuckDB {} by its absolute path",
            observe::DUCKDB_VERSION,
            pin.url(),
            pin.sha256,
            to.display(),
            observe::DUCKDB,
            observe::DUCKDB_VERSION
        )
    };
    let dir = to.parent().ok_or_else(|| failed("it has no directory".to_owned()))?;
    let scratch = dir.join("fetched");
    fs::create_dir_all(&scratch).map_err(|e| failed(format!("{}: {e}", scratch.display())))?;
    let archive = fetch(pin, &scratch).map_err(&failed)?;
    unpack(&archive, &to).map_err(|e| failed(format!("unpacking {}: {e}", archive.display())))?;
    // The archive is the store's now; the copy written out is not needed again.
    let _ = blaze_fetch::unpack::remove_all(&scratch);
    Ok(to)
}

/// The gzip `archive` (one member: the program) written to `to`, executable: to a file beside
/// it first, renamed over, so `to` is whole or absent, and two commands at once each rename the
/// same bytes.
fn unpack(archive: &Path, to: &Path) -> io::Result<()> {
    // alloc: once per first use
    let part = to.with_extension(format!("part-{}", std::process::id()));
    let written = (|| {
        let mut from = flate2::read::GzDecoder::new(fs::File::open(archive)?);
        let mut file = fs::File::create(&part)?;
        io::copy(&mut from, &mut file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        // durable: review: the unpacked DuckDB is a download unpacked again when missing; synced so its name never holds a torn program, which nothing checks before running it
        file.sync_all()?;
        fs::rename(&part, to)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&part);
    }
    written
}

/// `pin`'s archive, fetched by the daemon into its store against the pin's SHA-256 and written
/// out under `scratch`: the path of that file.
fn fetch(pin: &Pin, scratch: &Path) -> Result<PathBuf, String> {
    let out = scratch
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", scratch.display()))? // alloc: error path
        .to_owned(); // alloc: once per first use
    let request = Request::FetchUrl {
        url: Some(pin.url()),
        repo: None,
        rev: None,
        digest: format!("sha256:{}", pin.sha256), // alloc: once per first use
        most_bytes: Some(MOST_BYTES),
        out: Some(out),
    };
    let mut stored = None;
    ask(request, Start::IfMissing, |response| {
        if let ResponseRef::Stored { path: Some(path), .. } = response {
            stored = Some(PathBuf::from(path));
        }
        Ok(())
    })?;
    stored.ok_or_else(|| "the daemon answered with nothing written out".to_owned())
}

/// The DuckDB every reader of the history runs: [`locate`] with this process's
/// [`observe::DUCKDB`], the daemon's state directory and this host's pin.
pub(super) fn path() -> Result<PathBuf, String> {
    let state: PathBuf = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    locate(std::env::var_os(observe::DUCKDB), &state, Pin::host(), fetch)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Write as _};

    use super::*;

    const PIN: Pin = Pin { asset: "duckdb_cli-test.gz", sha256: "00ff" };

    fn gz(dir: &Path, bytes: &[u8]) -> PathBuf {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(bytes).unwrap();
        let path = dir.join("archive.gz");
        fs::write(&path, gz.finish().unwrap()).unwrap();
        path
    }

    /// The order: the variable, then what is laid out, then the fetch, once; and each way it
    /// cannot be had is an error naming the pin or the variable.
    #[test]
    fn duckdb_is_the_variables_then_the_pin_laid_out_then_the_pin_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let never = |_: &Pin, _: &Path| -> Result<PathBuf, String> { panic!("fetched") };

        // 1. The variable wins, with nothing fetched; set wrong, it is the error.
        let named = dir.path().join("named-duckdb");
        fs::write(&named, b"").unwrap();
        let found = locate(Some(named.clone().into()), &state, Some(&PIN), never);
        assert_eq!(found, Ok(named));
        let wrong = locate(Some("bin/duckdb".into()), &state, Some(&PIN), never).unwrap_err();
        assert!(wrong.contains("BLAZE_DUCKDB=bin/duckdb"), "{wrong}");

        // 3. Unset and not laid out: fetched once, unpacked, executable.
        let fetches = Cell::new(0);
        let archive = gz(dir.path(), b"#!/bin/sh\necho duckdb\n");
        let fetch = |pin: &Pin, scratch: &Path| {
            fetches.set(fetches.get() + 1);
            assert_eq!(pin.sha256, "00ff");
            assert!(scratch.is_dir(), "the scratch directory is made before the fetch");
            Ok(archive.clone()) // clone: a test's path
        };
        let fetched = locate(None, &state, Some(&PIN), fetch).unwrap();
        assert_eq!(fetched, PIN.laid_out(&state));
        assert!(fetched.starts_with(&state), "{}", fetched.display());
        assert_eq!(fs::read(&fetched).unwrap(), b"#!/bin/sh\necho duckdb\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&fetched).unwrap().permissions().mode() & 0o111, 0o111);
        }
        assert_eq!(fetches.get(), 1);

        // 2. Laid out: found again with no fetch.
        assert_eq!(locate(None, &state, Some(&PIN), never), Ok(fetched.clone())); // clone: a test's path

        // Another pin is another place: the old one is never taken for it.
        let other = Pin { asset: "duckdb_cli-test.gz", sha256: "11ee" };
        assert_ne!(other.laid_out(&state), fetched);

        // A fetch that fails names the pin and the variable, and leaves nothing at the path.
        let empty = dir.path().join("empty-state");
        let refused = |_: &Pin, _: &Path| Err("sha256 differs: got abcd".to_owned());
        let said = locate(None, &empty, Some(&PIN), refused).unwrap_err();
        for part in [observe::DUCKDB_VERSION, "sha256:00ff", "sha256 differs: got abcd", "BLAZE_DUCKDB"]
        {
            assert!(said.contains(part), "{part}: {said}");
        }
        assert!(said.contains(&PIN.url()), "{said}");
        assert!(!PIN.laid_out(&empty).exists());

        // An archive that is not gzip is the same error, with no part file left.
        let junk = dir.path().join("junk.gz");
        fs::write(&junk, b"not gzip").unwrap();
        let junk_state = dir.path().join("junk-state");
        let said =
            locate(None, &junk_state, Some(&PIN), |_: &Pin, _: &Path| Ok(junk.clone())) // clone: a test's path
                .unwrap_err();
        assert!(said.contains("unpacking"), "{said}");
        let left: Vec<_> = fs::read_dir(PIN.laid_out(&junk_state).parent().unwrap())
            .unwrap()
            .map(|it| it.unwrap().file_name())
            .filter(|it| it != "fetched")
            .collect();
        assert!(left.is_empty(), "{left:?}");

        // No pin for the host: the error names the variable.
        let none = locate(None, &state, None, never).unwrap_err();
        assert!(none.contains("pins no DuckDB") && none.contains("BLAZE_DUCKDB"), "{none}");
    }

    /// The pin of every host a release is built for is a SHA-256 and a `.gz` of DuckDB's CLI.
    #[test]
    fn every_pin_is_a_sha256_of_a_gz() {
        for (_, pin) in observe::DUCKDB_PINS {
            assert_eq!(pin.sha256.len(), 64, "{pin:?}");
            assert!(pin.sha256.bytes().all(|b| b.is_ascii_hexdigit()), "{pin:?}");
            assert!(pin.asset.starts_with("duckdb_cli-") && pin.asset.ends_with(".gz"), "{pin:?}");
            assert!(pin.url().contains(observe::DUCKDB_VERSION), "{pin:?}");
        }
    }
}
