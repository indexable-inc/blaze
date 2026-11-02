//! `blaze doc`'s side of the client: the daemon checks what `cargo doc` documents and makes the
//! pages from its front end (`serve::unit::doc`), no rustdoc run; this writes them where cargo
//! doc writes rustdoc's, says what cargo doc says once they are written, and opens them for
//! `--open` (`ops::cargo_doc::doc`).

use std::{
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use blaze_api::Documented;

use super::say;

/// The documentation as its responses come: where it goes, then each page.
#[derive(Default)]
pub struct Docs {
    documented: Option<Documented>,
    /// What went wrong first: the pages after it are not written.
    failed: Option<String>,
}

impl Docs {
    pub fn documented(&mut self, documented: Documented) {
        if self.documented.is_some() {
            self.failed.get_or_insert_with(|| "the daemon said twice where the documentation goes".to_owned());
            return;
        }
        self.documented = Some(documented);
    }

    /// A page, written under [`Documented::dir`].
    pub fn page(&mut self, path: &str, page: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        if let Err(what) = self.write(path, page) {
            self.failed = Some(what);
        }
    }

    fn write(&self, path: &str, page: &[u8]) -> Result<(), String> {
        let Some(documented) = &self.documented else {
            return Err(format!("the daemon sent the page `{path}` before saying where the documentation goes"));
        };
        let to = under(Path::new(&documented.dir), path)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        std::fs::write(&to, page).map_err(|error| format!("writing {}: {error}", to.display()))
    }

    /// Once every page is written: `crates.js`, which names the crates earlier runs documented in
    /// the same directory too, those whose pages are still there. What was documented, or the
    /// first thing that went wrong.
    pub fn finish(self) -> Result<Documented, String> {
        if let Some(failed) = self.failed {
            return Err(failed);
        }
        let Some(documented) = self.documented else {
            return Err("the daemon answered `doc` with no documentation".to_owned());
        };
        let dir = Path::new(&documented.dir);
        let list = dir.join("crates.js");
        let earlier = match std::fs::read_to_string(&list) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("reading {}: {error}", list.display())),
        };
        let names: Vec<&str> = documented.crates.iter().map(|it| it.name.as_str()).collect();
        let kept = |name: &str| under(dir, name).is_ok_and(|it| it.join("index.html").is_file());
        let text = blaze_doc::html::crates_list(earlier.as_deref(), kept, &names);
        std::fs::write(&list, text).map_err(|error| format!("writing {}: {error}", list.display()))?;
        Ok(documented)
    }
}

/// `path`, `/`-separated, under `dir`: refused unless each part of it is a name, so that what
/// the daemon sends lands under `dir` and nowhere else.
fn under(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let rel = Path::new(path);
    let named = !path.is_empty() && rel.components().all(|it| matches!(it, Component::Normal(_)));
    match named {
        true => Ok(dir.join(rel)),
        false => Err(format!("the daemon sent the page `{path}`, which is not under {}", dir.display())),
    }
}

/// What cargo doc says once the pages are written: `Generated` of the first crate asked for's
/// front page, and how many others were asked for; with `--open`, `Opening` it instead, and it
/// opened as cargo opens it.
pub fn done(documented: &Documented, open: bool, quiet: bool, color: bool, err: &mut impl Write) -> io::Result<()> {
    let mut asked = documented.crates.iter().filter(|it| it.asked);
    let Some(first) = asked.next() else { return Ok(()) };
    let page = Path::new(&documented.dir).join(&first.name).join("index.html");
    if open {
        if !quiet {
            err.write_all(say::status("Opening", &page.display().to_string(), color).as_bytes())?;
        }
        return match opened(&page) {
            Ok(()) => Ok(()),
            Err(why) => err.write_all(say::warning(&format!("failed to open: {why}"), color).as_bytes()),
        };
    }
    if quiet {
        return Ok(());
    }
    let shown = match asked.count() {
        0 => page.display().to_string(),
        1 => format!("{} and 1 other file", page.display()),
        others => format!("{} and {others} other files", page.display()),
    };
    err.write_all(say::status("Generated", &shown, color).as_bytes())
}

/// `page` opened with `$BROWSER`, else the system's opener, as cargo's `open_docs` does.
fn opened(page: &Path) -> Result<(), String> {
    let browser = std::env::var_os("BROWSER").filter(|it| !it.is_empty());
    let opener = match &browser {
        Some(browser) => browser.clone(),
        None if cfg!(target_os = "macos") => "open".into(),
        None if cfg!(windows) => "explorer".into(),
        None => "xdg-open".into(),
    };
    let status = std::process::Command::new(&opener).arg(page).status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{} exited with {status}", opener.to_string_lossy())),
        Err(error) => Err(format!("{}: {error}", opener.to_string_lossy())),
    }
}

#[cfg(test)]
mod tests {
    use super::under;
    use std::path::Path;

    #[test]
    fn a_page_lands_under_the_documentation_alone() {
        let dir = Path::new("/w/target/doc");
        assert_eq!(under(dir, "app/index.html").unwrap(), Path::new("/w/target/doc/app/index.html"));
        assert!(under(dir, "../escape.html").is_err());
        assert!(under(dir, "/etc/passwd").is_err());
        assert!(under(dir, "app/../../x").is_err());
        assert!(under(dir, "./app").is_err());
        assert!(under(dir, "").is_err());
    }
}
