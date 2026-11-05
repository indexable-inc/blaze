//! Opening what a view links to (`open:` actions): a web link in the browser, a file in the app the system gives its kind,
//! at a line when one is said (a cell's FileRef) and that app takes one. Each editor takes a line its own way (its URL
//! scheme, or Xcode's `xed`), found by the app's bundle id; an app that takes none opens the file at its top, and the log
//! says which app it was.

/// What a press on an `open:` action opens: `open:<url>`, `open:<path>`, or `open:<line>:<path>` (a path is absolute, so
/// the line before it is unambiguous). `reveal` (an Option-press) shows a file in the Finder instead of opening it.
pub fn open(target: &str, reveal: bool) {
    let (path, line) = match target.split_once(':') {
        Some((n, rest)) if rest.starts_with('/') && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => (rest, n.parse::<u32>().ok()),
        _ => (target, None),
    };
    let cmd = if path.starts_with("https://") || path.starts_with("http://") {
        Some(Launch::Open(vec![path.to_owned()]))
    } else if path.starts_with('/') && reveal {
        Some(Launch::Open(vec!["-R".to_owned(), path.to_owned()]))
    } else if path.starts_with('/') {
        Some(file(path, line))
    } else {
        eprintln!("harness-native: open {target:?}: not a web link or an absolute path");
        None
    };
    let Some(cmd) = cmd else { return };
    let (prog, args) = match &cmd {
        Launch::Open(args) => ("/usr/bin/open", args),
        Launch::Xed(args) => ("/usr/bin/xed", args),
    };
    if let Err(e) = std::process::Command::new(prog).args(args).spawn() {
        eprintln!("harness-native: open {target}: {prog}: {e}");
    }
}

/// A program to run for an open: `/usr/bin/open` or Xcode's `/usr/bin/xed`, with its arguments.
#[derive(Debug, PartialEq, Eq)]
enum Launch {
    Open(Vec<String>),
    Xed(Vec<String>),
}

/// How to open the file at `path` (absolute), at `line` when given: one that would run (an executable, an app) is shown
/// in the Finder instead; else the app the system gives its kind opens it, at the line when that app takes one.
fn file(path: &str, line: Option<u32>) -> Launch {
    use std::os::unix::fs::PermissionsExt;
    let runs = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        || [".app", ".command", ".tool", ".terminal"].iter().any(|x| path.trim_end_matches('/').ends_with(x));
    if runs {
        return Launch::Open(vec!["-R".to_owned(), path.to_owned()]);
    }
    let Some(line) = line else { return Launch::Open(vec![path.to_owned()]) };
    let app = default_app(path);
    match app.as_deref().and_then(|id| at_line(id, path, line)) {
        Some(l) => l,
        None => {
            eprintln!(
                "harness-native: open {path}:{line}: {} takes no line, so it opens at the top",
                app.as_deref().unwrap_or("the app for its kind (none found)")
            );
            Launch::Open(vec![path.to_owned()])
        }
    }
}

/// How the app with bundle id `app` opens `path` at `line`, when it takes a line: its URL scheme, or `xed --line`.
fn at_line(app: &str, path: &str, line: u32) -> Option<Launch> {
    let file_url = format!("file://{}", escape(path));
    let url = match app {
        "dev.zed.Zed" | "dev.zed.Zed-Preview" => format!("zed://file{}:{line}", escape(path)),
        "com.microsoft.VSCode" => format!("vscode://file{}:{line}", escape(path)),
        "com.microsoft.VSCodeInsiders" => format!("vscode-insiders://file{}:{line}", escape(path)),
        "com.todesktop.230313mzl4w4u92" => format!("cursor://file{}:{line}", escape(path)),
        "com.exafunction.windsurf" => format!("windsurf://file{}:{line}", escape(path)),
        "com.sublimetext.4" | "com.sublimetext.3" => format!("subl://open?url={}&line={line}", escape(&file_url)),
        "com.barebones.bbedit" => format!("x-bbedit://open?url={}&line={line}", escape(&file_url)),
        "com.macromates.TextMate" => format!("txmt://open?url={}&line={line}", escape(&file_url)),
        "com.apple.dt.Xcode" => return Some(Launch::Xed(vec!["--line".to_owned(), line.to_string(), path.to_owned()])),
        _ => return None,
    };
    Some(Launch::Open(vec![url]))
}

/// `s` percent-encoded for a URL, keeping `/` and the unreserved characters.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The bundle id of the app the system opens the file at `path` with (Launch Services, through NSWorkspace).
#[cfg(target_os = "macos")]
fn default_app(path: &str) -> Option<String> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let app = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&url)?;
    Some(NSBundle::bundleWithURL(&app)?.bundleIdentifier()?.to_string())
}

#[cfg(not(target_os = "macos"))]
fn default_app(_path: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each editor gets the line its own way; a path with a space stays one; an app that takes no line gets none.
    #[test]
    fn editors_open_at_the_line() {
        let open = |s: &str| Some(Launch::Open(vec![s.to_owned()]));
        assert_eq!(at_line("dev.zed.Zed", "/x/a b.rs", 60), open("zed://file/x/a%20b.rs:60"));
        assert_eq!(at_line("com.todesktop.230313mzl4w4u92", "/x/t.ts", 7), open("cursor://file/x/t.ts:7"));
        assert_eq!(at_line("com.sublimetext.4", "/x/t.ts", 7), open("subl://open?url=file%3A///x/t.ts&line=7"));
        assert_eq!(at_line("com.apple.dt.Xcode", "/x/t.swift", 3), Some(Launch::Xed(vec!["--line".into(), "3".into(), "/x/t.swift".into()])));
        assert_eq!(at_line("com.apple.TextEdit", "/x/t.txt", 3), None);
    }
}
