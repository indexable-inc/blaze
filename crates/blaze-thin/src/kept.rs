//! The one writer of a solo answer's rows ([`keep`]): the record, its layout, its directory
//! and the daemon's take of them are blaze-certify's (`blaze_certify::kept`), their one home.

use blaze_certify::kept::{DIR, Kept, names};

/// `kept` written to `<state>/solo-rows/<pid>`: whole under its `.part` name, then renamed,
/// so a reader sees the record whole or not at all. The directory is made (this user's alone)
/// when it is not there. An error names the path and the step.
pub fn keep(state: &std::path::Path, kept: &Kept) -> Result<std::path::PathBuf, String> {
    use std::{
        io::Write as _,
        os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    };
    let bytes = kept.write()?;
    let dir = state.join(DIR); // alloc: the directory's path, once a solo answer
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("making {}: {error}", dir.display())),
    }
    let (name, part) = names(kept.pid);
    let (path, part) = (dir.join(name), dir.join(part)); // alloc: two paths, once a solo answer
    let io = |what: &str, at: &std::path::Path, error: std::io::Error| format!("{what} {}: {error}", at.display());
    // `truncate`: a `.part` of this pid is an earlier process's of the same id, which died
    // before its rename; it is this process's to write over.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)
        .map_err(|error| io("making", &part, error))?;
    file.write_all(&bytes).map_err(|error| io("writing", &part, error))?;
    drop(file);
    std::fs::rename(&part, &path).map_err(|error| io("renaming into place", &path, error))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: what [`keep`] writes is one whole file under the record's name, which
    /// reads back as the record, and no `.part` is left. Control: the bytes read back are
    /// compared whole, so a record written without its rename (only `<pid>.part`) fails the
    /// read of `<pid>`.
    #[test]
    fn a_kept_record_is_one_whole_file_and_no_part() {
        let state = std::env::temp_dir().join(format!("blaze-thin-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).unwrap();
        let record = Kept {
            pid: 41,
            started_ns: 10,
            ended_ns: 90,
            certified: Some(7),
            slot: "ab12".to_owned(),
            workspace: "/w".to_owned(),
            command: "run".to_owned(),
            timed: vec![blaze_api::Timed { name: crate::talk::SOLO_CHECK.to_owned(), start_ns: 20, end_ns: 60 }],
        };
        let path = keep(&state, &record).unwrap();
        assert_eq!(path, state.join(DIR).join("41"));
        assert_eq!(Kept::read(&std::fs::read(&path).unwrap()), Ok(record));
        assert!(!state.join(DIR).join("41.part").exists());
        let _ = std::fs::remove_dir_all(&state);
    }
}
