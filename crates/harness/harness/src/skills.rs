//! Skills: the instructions for one kind of work (Andrew's Gmail, GitHub, cells as UI, a project's own build rules),
//! kept out of the system note until an agent's task needs them. The note carries only their index, one line each,
//! generated here from the files ([`Skills::index`]); the full text loads on demand through the kernel's
//! `skill(name)` (mcp/src/skills.ts), which gets the same set from the launch as `HARNESS_SKILLS` ([`Skills::env`]), so
//! the index an agent reads and the skills it can load are one list.
//!
//! A skill is a Markdown file with front matter, the format Claude Code and Codex skills use:
//!
//! ```text
//! ---
//! name: github
//! description: One line: what it covers and when to read it.
//! ---
//! The body.
//! ```
//!
//! laid out as `<dir>/<name>.md`, or `<dir>/<name>/SKILL.md` for a folder skill whose body names files beside it. The
//! front matter's `name` is the file's (or folder's) name, lower-case letters, digits and `-`; `name` and
//! `description` are one line each, plain, 'single-' or "double-quoted". Other keys (Claude's `license`,
//! `allowed-tools`, `metadata`, ...) are skipped. A skill that breaks these rules stops the launch with an error naming
//! its file and the rule.
//!
//! Where skills come from, a later source overriding an earlier one by name ([`Skills::discover`]):
//! 1. harness: `<kernel root>/skills` (crates/harness/mcp/skills), shipped beside the kernel that serves them, so
//!    `HARNESS_ROOT` moves both and an edit needs no rebuild.
//! 2. user: `$HARNESS_HOME/skills` (~/.harness/skills): harness's own, never `~/.claude/skills`, which the locked-down
//!    launch keeps out on purpose (profiles.rs).
//! 3. project: `.agents/skills` and `.claude/skills` at the git root of the session's directory (blaze's own skills),
//!    the same file through both counted once.
use crate::paths;
use anyhow::{Context, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a skill came from (the module doc's list, in its order of precedence).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Harness,
    User,
    Project,
}

/// One skill as the index and the kernel see it; its body stays in its file until `skill(name)` reads it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: Source,
    /// The Markdown file: `<name>.md`, or a folder skill's `SKILL.md`.
    pub path: PathBuf,
}

/// The skills one launch offers, by name.
#[derive(Clone, Debug, Default)]
pub struct Skills(Vec<Skill>);

/// The index's opening: what a skill is and how to load one. The lines after it are one per skill.
const INDEX_HEAD: &str = "Skills are the full instructions for one kind of work, kept out of this note until a task \
needs them. Before work a skill covers, read it in ts with await skill('name'), which returns its text (the person sees \
it as a quiet step); skills() lists them with their files. The skills:";

impl Skills {
    /// The skills for a session in `cwd`, from every source in the module doc.
    ///
    /// # Errors
    /// The kernel's root is unknown, its skills folder can't be read, or a skill breaks a rule (named with its file).
    pub fn discover(cwd: &Path) -> anyhow::Result<Self> {
        let mut dirs = vec![(Source::Harness, paths::root()?.join("skills")), (Source::User, paths::home().join("skills"))];
        if let Some(git) = cwd.ancestors().find(|d| d.join(".git").exists()) {
            dirs.push((Source::Project, git.join(".agents/skills")));
            dirs.push((Source::Project, git.join(".claude/skills")));
        }
        Self::from_dirs(&dirs)
    }

    /// The skills in these folders, a later folder's skill replacing an earlier one's of the same name from another source.
    fn from_dirs(dirs: &[(Source, PathBuf)]) -> anyhow::Result<Self> {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        for (source, dir) in dirs {
            for skill in read_dir(*source, dir)? {
                match by_name.get(&skill.name) {
                    // The same file through two folders (blaze's .claude/skills links to .agents/skills).
                    Some(had) if same_file(&had.path, &skill.path) => {}
                    Some(had) if had.source == skill.source => bail!(
                        "two {source:?} skills named {:?}: {} and {}; a name means one skill, so rename or remove one",
                        skill.name,
                        had.path.display(),
                        skill.path.display()
                    ),
                    _ => {
                        by_name.insert(skill.name.clone(), skill);
                    }
                }
            }
        }
        Ok(Self(by_name.into_values().collect()))
    }

    /// The skills, by name.
    #[cfg(test)]
    fn list(&self) -> &[Skill] {
        &self.0
    }

    /// The system note's skills part: [`INDEX_HEAD`], then `- name: description` per skill, by name.
    #[must_use]
    pub fn index(&self) -> String {
        let mut s = String::from(INDEX_HEAD);
        for k in &self.0 {
            s.push_str("\n- ");
            s.push_str(&k.name);
            s.push_str(": ");
            s.push_str(&k.description);
        }
        s
    }

    /// `HARNESS_SKILLS` for the kernel: the list as JSON, `[{ name, description, source, path }]`.
    #[must_use]
    pub fn env(&self) -> String {
        serde_json::to_string(&self.0).expect("skills are plain strings")
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The skills directly in `dir`, by file name. A missing folder has none, except the kernel's own, which must exist.
fn read_dir(source: Source, dir: &Path) -> anyhow::Result<Vec<Skill>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && source != Source::Harness => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("skills: reading the {source:?} skills folder {}: {e}", dir.display())),
    };
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("skills: listing {}", dir.display()))?;
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let file = path.join("SKILL.md");
            // A folder without SKILL.md is not a skill (a folder of shared references, say).
            if file.is_file() {
                found.push((file_name, file));
            }
        } else if let Some(stem) = file_name.strip_suffix(".md") {
            found.push((stem.to_owned(), path));
        }
    }
    found.sort();
    let mut out = Vec::with_capacity(found.len());
    for (name, path) in found {
        let text = std::fs::read_to_string(&path).with_context(|| format!("skill {}: reading it", path.display()))?;
        let fm = front_matter(&text).map_err(|why| anyhow!("skill {}: {why}", path.display()))?;
        if fm.name != name {
            bail!(
                "skill {}: its front matter names it {:?}, but a skill's name is its file's or folder's name, {name:?}",
                path.display(),
                fm.name
            );
        }
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            bail!("skill {}: its name {name:?} must be lower-case letters, digits and '-'", path.display());
        }
        out.push(Skill { name, description: fm.description, source, path });
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
struct FrontMatter {
    name: String,
    description: String,
}

/// The `name` and `description` of a skill file's front matter, or the rule it breaks.
fn front_matter(text: &str) -> Result<FrontMatter, String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err("no front matter: a skill starts with a `---` line, then `name:` and `description:` lines, then `---`".into());
    }
    let (mut name, mut description) = (None, None);
    let mut last: Option<&str> = None;
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        // A blank line, a comment, or a nested or continued value (under a key we skip, such as `metadata:`).
        if line.trim().is_empty() || line.starts_with([' ', '\t', '#', '-']) {
            if let Some(key @ ("name" | "description")) = last
                && !line.trim().is_empty()
            {
                return Err(format!("`{key}` must be one line"));
            }
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| format!("front matter line {line:?} is not `key: value`"))?;
        let key = key.trim();
        last = Some(key);
        let slot = match key {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        if slot.is_some() {
            return Err(format!("`{key}` is given twice"));
        }
        *slot = Some(scalar(key, value.trim())?);
    }
    if !closed {
        return Err("its front matter has no closing `---` line".into());
    }
    match (name, description) {
        (Some(name), Some(description)) => Ok(FrontMatter { name, description }),
        (None, _) => Err("its front matter has no `name:`".into()),
        (_, None) => Err("its front matter has no `description:` (one line: what it covers and when to read it)".into()),
    }
}

/// One front matter value: plain, 'single-quoted' ('' is a quote) or "double-quoted" (JSON's escapes), never empty.
fn scalar(key: &str, v: &str) -> Result<String, String> {
    let s = if v.starts_with('"') {
        serde_json::from_str::<String>(v).map_err(|e| format!("`{key}`: {v} is not one closed double-quoted string ({e})"))?
    } else if let Some(inner) = v.strip_prefix('\'') {
        inner.strip_suffix('\'').ok_or_else(|| format!("`{key}`: {v} has no closing quote"))?.replace("''", "'")
    } else if v.is_empty() || v.starts_with(['|', '>']) {
        return Err(format!("`{key}` must be a one-line value on its own line"));
    } else {
        v.to_owned()
    };
    if s.trim().is_empty() || s.contains('\n') {
        return Err(format!("`{key}` must be one non-empty line"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// harness's own skills (mcp/skills) all load, so a launch never stops on one, and each gives the index one line.
    #[test]
    fn harness_skills_load() {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../mcp/skills"));
        let skills = Skills::from_dirs(&[(Source::Harness, dir)]).unwrap();
        let names: Vec<&str> = skills.list().iter().map(|s| s.name.as_str()).collect();
        for want in ["github", "gmail", "ui"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        let index = skills.index();
        assert_eq!(index.lines().count(), 1 + skills.list().len(), "one line per skill:\n{index}");
    }

    /// The front matter rules, each refusal naming the rule.
    #[test]
    fn front_matter_rules() {
        let fm = |s: &str| front_matter(s);
        assert_eq!(
            fm("---\nname: a\ndescription: \"Say \\\"hi\\\"\"\nlicense: MIT\nmetadata:\n  k: v\n---\nbody"),
            Ok(FrontMatter { name: "a".into(), description: "Say \"hi\"".into() })
        );
        assert_eq!(fm("---\nname: 'it''s'\ndescription: x\n---\n").unwrap().name, "it's");
        assert!(fm("# no front matter").unwrap_err().contains("no front matter"));
        assert!(fm("---\nname: a\ndescription: x\n").unwrap_err().contains("closing"));
        assert!(fm("---\nname: a\n---\n").unwrap_err().contains("description"));
        assert!(fm("---\nname: a\ndescription: >\n  folded\n---\n").unwrap_err().contains("one-line"));
        assert!(fm("---\nname: a\ndescription: one\n  two\n---\n").unwrap_err().contains("one line"));
        assert!(fm("---\nname: a\nname: b\ndescription: x\n---\n").unwrap_err().contains("twice"));
    }
}
