//! Instructions: the AGENTS.md files that tell an agent a project's rules, loaded by harness into the system note
//! for every agent kind alike (claude, codex, cursor), with the agents' own loaders switched off (profiles.rs:
//! `CLAUDE_CODE_DISABLE_CLAUDE_MDS`, codex's `project_doc_max_bytes=0`). One loader means one answer to "what was
//! this agent told", the same for every kind, shown and counted where the note is.
//!
//! Which files, in this order (the note carries each whole, under a heading naming its path):
//! 1. user: `$HARNESS_HOME/AGENTS.md` (~/.harness/AGENTS.md): harness's own, never `~/.claude` or `~/.codex`;
//! 2. project: `AGENTS.md` in the session directory's git root, then in each directory from there down to the session
//!    directory, so a nearer file comes later and reads as the more specific one.
//!
//! A file under the git root that is not on that path (`crates/x/AGENTS.md` for a session at the root) is not loaded:
//! the note says to read the one beside the files being changed. A file past [`MAX_BYTES`] stops the launch, naming
//! it: a rule file that size is a skill's worth of text, and cutting it short would tell the agent half its rules.
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::paths;

/// The file's name, in every directory.
pub const FILE: &str = "AGENTS.md";

/// The most one file may hold.
pub const MAX_BYTES: usize = 64 * 1024;

/// The line after the files: where the rest of a project's rules are.
const NESTED: &str = "A directory below the git root may have its own AGENTS.md with rules for the files under it: read it before changing files there.";

/// One loaded file.
#[derive(Debug, PartialEq, Eq)]
pub struct File {
    /// Its path, as the note's heading shows it: from the git root for a project file, `~/.harness/AGENTS.md` for the user's.
    pub shown: String,
    pub text: String,
}

/// The files for one session.
#[derive(Debug, PartialEq, Eq)]
pub struct Instructions(pub Vec<File>);

impl Instructions {
    /// The files for a session in `cwd`, from the sources in the module doc.
    ///
    /// # Errors
    /// A file that can't be read, isn't UTF-8 or is past [`MAX_BYTES`], named.
    pub fn discover(cwd: &Path) -> anyhow::Result<Self> {
        Self::from(&paths::home(), cwd)
    }

    fn from(home: &Path, cwd: &Path) -> anyhow::Result<Self> {
        let mut files = Vec::new();
        if let Some(text) = read(&home.join(FILE))? {
            files.push(File { shown: format!("~/.harness/{FILE}"), text });
        }
        if let Some(git) = cwd.ancestors().find(|d| d.join(".git").exists()) {
            // From the root down to cwd: ancestors() goes up, so collect and reverse.
            let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|d| d.starts_with(git)).collect();
            dirs.reverse();
            for dir in dirs {
                let path = dir.join(FILE);
                if let Some(text) = read(&path)? {
                    let shown = path.strip_prefix(git).map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
                    files.push(File { shown, text });
                }
            }
        }
        Ok(Self(files))
    }

    /// The system note's instructions part, or "" when there are none.
    #[must_use]
    pub fn note(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let mut s = String::new();
        for f in &self.0 {
            s.push_str(&format!("# Instructions: {}

{}

", f.shown, f.text.trim()));
        }
        s.push_str(NESTED);
        s
    }
}

/// `path`'s text, `None` when there is no such file.
fn read(path: &PathBuf) -> anyhow::Result<Option<String>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if bytes.len() > MAX_BYTES {
        bail!(
            "{} is {} bytes, past the {MAX_BYTES} an instructions file may hold: move the detail into skills and keep the file to its rules",
            path.display(),
            bytes.len()
        );
    }
    String::from_utf8(bytes).map(Some).with_context(|| format!("{} is not UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("harness-instructions-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("repo/.git")).unwrap();
        std::fs::create_dir_all(d.join("repo/crates/a/src")).unwrap();
        std::fs::create_dir_all(d.join("repo/crates/b")).unwrap();
        std::fs::create_dir_all(d.join("home")).unwrap();
        d
    }

    /// The user's file, then the root's, then each on the way down to cwd; one off the path is left out.
    #[test]
    fn root_to_cwd_in_order() {
        let d = tree("order");
        std::fs::write(d.join("home/AGENTS.md"), "user").unwrap();
        std::fs::write(d.join("repo/AGENTS.md"), "root").unwrap();
        std::fs::write(d.join("repo/crates/a/AGENTS.md"), "a").unwrap();
        std::fs::write(d.join("repo/crates/b/AGENTS.md"), "b, not on the path").unwrap();
        let got = Instructions::from(&d.join("home"), &d.join("repo/crates/a/src")).unwrap();
        let shown: Vec<(&str, &str)> = got.0.iter().map(|f| (f.shown.as_str(), f.text.as_str())).collect();
        assert_eq!(shown, [("~/.harness/AGENTS.md", "user"), ("AGENTS.md", "root"), ("crates/a/AGENTS.md", "a")]);
        let note = got.note();
        assert!(note.find("# Instructions: AGENTS.md").unwrap() < note.find("# Instructions: crates/a/AGENTS.md").unwrap());
        assert!(note.ends_with(NESTED));
    }

    /// No files: no part at all. Outside a repository only the user's file counts.
    #[test]
    fn none_and_outside_a_repo() {
        let d = tree("none");
        assert_eq!(Instructions::from(&d.join("home"), &d.join("repo")).unwrap().note(), "");
        std::fs::write(d.join("home/AGENTS.md"), "user").unwrap();
        std::fs::write(d.join("AGENTS.md"), "above the repo, not loaded").unwrap();
        let got = Instructions::from(&d.join("home"), &d).unwrap();
        assert_eq!(got.0.len(), 1);
    }

    /// A file past the cap stops the launch, naming it, instead of being cut.
    #[test]
    fn too_big_is_an_error() {
        let d = tree("big");
        std::fs::write(d.join("repo/AGENTS.md"), "x".repeat(MAX_BYTES + 1)).unwrap();
        let err = Instructions::from(&d.join("home"), &d.join("repo")).unwrap_err().to_string();
        assert!(err.contains("AGENTS.md") && err.contains("past the"), "{err}");
    }

    /// blaze's own AGENTS.md fits.
    #[test]
    fn blazes_file_fits() {
        let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.."));
        let got = Instructions::from(&std::env::temp_dir().join("harness-no-home"), &root).unwrap();
        assert!(got.0.iter().any(|f| f.shown == "AGENTS.md"), "{:?}", got.0.iter().map(|f| &f.shown).collect::<Vec<_>>());
    }
}
