//! Whether this blaze is the one that runs where a command was typed, told with no manifest
//! read: from the lookup blaze-engine's launcher keeps (`crates/blaze-engine/src/pin.rs`,
//! `found`), which names the workspace's root manifest and what it pins and holds while every
//! manifest on the way has the stamp it had.
//!
//! The reader of that file is here, and the engine's launcher reads it with this one, so the
//! thin `blaze` and the engine can't read it two ways. Only the engine writes it: where none
//! is kept, or one of its stamps moved, the thin `blaze` knows nothing and hands the command
//! to the engine, whose launcher reads the manifests and keeps the lookup again.

use std::path::{Path, PathBuf};

/// A workspace found from a directory and what it pins.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    /// The root manifest; none outside any package.
    pub manifest: Option<PathBuf>,
    /// What it pins; none when it pins nothing.
    pub pinned: Option<String>,
}

/// What `stat` says of a manifest: while it says this, the manifest's text is what was read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub len: u64,
    /// Its modification time, in nanoseconds since the epoch.
    pub modified_ns: u128,
    pub inode: u64,
}

/// `path`'s stamp, `None` where no file is. `Err` is a file whose stamp can't be had: nothing
/// is kept of a lookup that met one.
pub fn stamp(path: &Path) -> Result<Option<Stamp>, ()> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(_) => return Err(()),
    };
    let modified = meta.modified().map_err(|_| ())?;
    let modified_ns =
        modified.duration_since(std::time::UNIX_EPOCH).map_err(|_| ())?.as_nanos();
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Ok(Some(Stamp { len: meta.len(), modified_ns, inode }))
}

/// The first line of a kept lookup: its format's name and version.
pub const KEPT: &str = "blaze pin lookup 1";

/// blaze's home, where what it installs and what its user says live: `$BLAZE_HOME`, else
/// `.blaze` in the user's home ([`blaze_core::home::decide`] of `var` and `record`, the
/// system's record of the user, asked only when no variable answers: a driver passes
/// [`blaze_core::home::record`]). The rule of `blaze_manifest::user_config::home`, which this
/// crate does not link (it brings cargo's manifest schema); blaze-engine's tests hold the two
/// to one answer.
///
/// # Errors
///
/// [`blaze_core::home::NoHome`] when `BLAZE_HOME` is unset and the user has no home.
pub fn home(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    record: impl FnOnce() -> Result<PathBuf, blaze_core::home::NoRecord>,
) -> Result<PathBuf, blaze_core::home::NoHome> {
    if let Some(home) = var("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    Ok(blaze_core::home::decide(&var, record)?.join(".blaze"))
}

/// Where the lookup from `start` is kept under blaze's `home`: `pins/<hash of start>`. None
/// for a directory that is not UTF-8 or has a tab or a newline in its name (the file's lines
/// could not name it): each lookup there reads the manifests.
pub fn kept_at(home: &Path, start: &Path) -> Option<PathBuf> {
    let text = start.to_str().filter(|it| !it.contains(['\t', '\n']))?;
    let name = blaze_format::hex::encode(&blaze_ids::b3::hash(text.as_bytes()));
    Some(home.join("pins").join(name))
}

/// The lookup from `start` as `file` keeps it, when every manifest it read, and every place it
/// found none, is as it was: a stat of each, and no manifest read or parsed. `None` when there
/// is none kept, one of them moved, or the file is not what the engine's launcher writes.
/// `names` says whether a pin's text names something a pin can (a version or a channel).
pub fn kept(file: &Path, start: &Path, names: impl Fn(&str) -> bool) -> Option<Found> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut lines = text.lines();
    if lines.next()? != KEPT || lines.next()?.strip_prefix("start\t")? != start.to_str()? {
        return None;
    }
    let (mut manifest, mut pin, mut whole) = (None, None, false);
    for line in lines {
        // Nothing follows the end.
        if whole {
            return None;
        }
        let mut parts = line.split('\t');
        match parts.next()? {
            "read" => {
                let path = Path::new(parts.next()?);
                let was = Stamp {
                    len: parts.next()?.parse().ok()?,
                    modified_ns: parts.next()?.parse().ok()?,
                    inode: parts.next()?.parse().ok()?,
                };
                if stamp(path) != Ok(Some(was)) {
                    return None;
                }
            }
            "none" => {
                if stamp(Path::new(parts.next()?)) != Ok(None) {
                    return None;
                }
            }
            "root" => manifest = Some(PathBuf::from(parts.next()?)),
            "pin" => {
                let version = parts.next()?;
                if !names(version) {
                    return None;
                }
                pin = Some(version.to_owned()); // alloc: once a lookup, the pin's text
            }
            "end" => whole = true,
            _ => return None,
        }
        if parts.next().is_some() {
            return None;
        }
    }
    // A pin is a root manifest's.
    (whole && (pin.is_none() || manifest.is_some())).then_some(Found { manifest, pinned: pin })
}

/// Why the thin `blaze` can't tell that this blaze runs where the command was typed: the
/// engine's launcher decides, and runs the release that does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unsure {
    /// No home says where the lookup is kept.
    NoHome,
    /// No lookup from the directory is kept, or a manifest on the way moved since.
    NotKept,
    /// The workspace pins another blaze than this one (a release, or a channel).
    Pins(String),
    /// Outside a pinned workspace, the default `blaze update` installed is another release, or
    /// its file can't be read.
    Default,
}

/// The workspace `start` is in, when this blaze (`ours`, its version) is known to be the one
/// that runs there: the lookup is kept and current, and it pins this one, or pins none while
/// `home` has no default release that is another (blaze-engine's `update::runs_unpinned`).
pub fn here(home: &Path, start: &Path, ours: &str) -> Result<Found, Unsure> {
    let file = kept_at(home, start).ok_or(Unsure::NotKept)?;
    // Whatever the pin's text is, it is compared to this blaze's version below.
    let found = kept(&file, start, |_| true).ok_or(Unsure::NotKept)?;
    match &found.pinned {
        Some(pinned) if pinned == ours => Ok(found),
        // clone: once a command, the reason it goes to the engine
        Some(pinned) => Err(Unsure::Pins(pinned.clone())),
        None => match std::fs::read_to_string(home.join("default")) {
            Ok(default) if default.trim() == ours => Ok(found),
            Ok(_) => Err(Unsure::Default),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(found),
            Err(_) => Err(Unsure::Default),
        },
    }
}

/// `--version`'s second line where no workspace is found from the directory: `this` is the
/// program asked.
pub fn says_no_workspace(this: &str) -> String {
    format!("{this}, as no workspace is here to pin one")
}

/// `--version`'s second line where the workspace's root manifest `shown` pins this blaze.
pub fn says_pinned(this: &str, shown: &Path) -> String {
    format!("{this}, as {} pins it", shown.display())
}

/// `--version`'s second line where the workspace's root manifest `shown` pins no blaze.
pub fn says_pins_none(this: &str, shown: &Path) -> String {
    format!(
        "{this}, as {} pins none (`[workspace.metadata.blaze] version`, which `blaze pin` writes)",
        shown.display()
    )
}

/// `--version`'s second line for a workspace found as [`here`] finds it.
pub fn says(this: &str, found: &Found) -> String {
    match (&found.manifest, &found.pinned) {
        (None, _) => says_no_workspace(this),
        (Some(manifest), Some(_)) => says_pinned(this, manifest),
        (Some(manifest), None) => says_pins_none(this, manifest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's under the system's temporary one, emptied.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-thin-pin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keep(home: &Path, start: &Path, lines: &str) {
        let file = kept_at(home, start).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text = format!("{KEPT}\nstart\t{}\n{lines}end\n", start.display());
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn nothing_kept_is_the_engines_to_decide() {
        let home = scratch("none");
        assert_eq!(here(&home, Path::new("/w"), "0.0.0-dev"), Err(Unsure::NotKept));
    }

    #[test]
    fn a_kept_lookup_holds_while_its_stamps_do_and_names_who_runs() {
        let home = scratch("kept");
        let start = home.join("w");
        std::fs::create_dir_all(&start).unwrap();
        let manifest = start.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"a\"\n").unwrap();
        let was = stamp(&manifest).unwrap().unwrap();
        let read = format!("read\t{}\t{}\t{}\t{}\n", manifest.display(), was.len, was.modified_ns, was.inode);
        let root = format!("root\t{}\n", manifest.display());
        keep(&home, &start, &format!("{read}{root}"));
        let found = here(&home, &start, "0.20261101.1").unwrap();
        assert_eq!(found, Found { manifest: Some(manifest.clone()), pinned: None });
        assert!(says("blaze", &found).ends_with("which `blaze pin` writes)"));
        // Another default installed: the engine runs it.
        std::fs::write(home.join("default"), "0.20261102.0\n").unwrap();
        assert_eq!(here(&home, &start, "0.20261101.1"), Err(Unsure::Default));
        std::fs::write(home.join("default"), "0.20261101.1\n").unwrap();
        assert!(here(&home, &start, "0.20261101.1").is_ok());
        // A pin: this blaze's, or another's.
        keep(&home, &start, &format!("{read}{root}pin\t0.20261101.1\n"));
        assert!(here(&home, &start, "0.20261101.1").is_ok());
        assert_eq!(here(&home, &start, "0.0.0-dev"), Err(Unsure::Pins("0.20261101.1".to_owned())));
        // The manifest grew: the lookup is no longer of it.
        std::fs::write(&manifest, "[package]\nname = \"a\"\nversion = \"1.0.0\"\n").unwrap();
        assert_eq!(here(&home, &start, "0.20261101.1"), Err(Unsure::NotKept));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_file_that_is_not_the_launchers_is_not_read_as_one() {
        let home = scratch("bad");
        let start = Path::new("/w");
        let file = kept_at(&home, start).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        for text in ["", "blaze pin lookup 0\nstart\t/w\nend\n", "blaze pin lookup 1\nstart\t/x\nend\n",
            "blaze pin lookup 1\nstart\t/w\n", "blaze pin lookup 1\nstart\t/w\nend\nroot\t/w/Cargo.toml\n",
            "blaze pin lookup 1\nstart\t/w\npin\t0.20261101.1\nend\n"]
        {
            std::fs::write(&file, text).unwrap();
            assert_eq!(kept(&file, start, |_| true), None, "{text:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
