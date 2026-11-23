//! D71: the host tools a native build script may start besides the pinned C toolchain (today:
//! `perl`, which sha3-asm starts to turn cryptogams' perlasm into assembly). The list's one home
//! is the pin table's `host_tools` (`pins/clang.json`, [`blaze_pins::CLANG_JSON`]).
//!
//! A script whose package is in a tool's `needed_by` is granted read and execute of the tool's
//! resolved real path and read-only of the folders its runtime needs; the tool's binary digest
//! is in its run key ([`crate::key::HostToolKey`]); its record is filed under
//! [`RUN_KIND_HOST_TOOL`], the `host-tool` mark, so it stays on this machine: the shared-cache
//! origin refuses to seal a record so marked. A tool not on the list is refused by the sandbox
//! as before (only granted programs may be started). Which tools a script gets is decided from
//! text (the table and the package's name) before the run; nothing searches `PATH` for it.
//! Sans-IO: the caller resolves and digests the path.
//!
//! D112: tikv-jemalloc-sys's script is granted `sh`, `make` (the real binary behind the xcrun
//! shim, never xcrun), `hostname` and `uname`; sh's `runs` are the programs its configure and
//! make's recipes start. A row's `runs_out_dir` ([`HostTool::runs_out_dir`], [`runs_out_dir`])
//! says the tool starts what the script wrote under OUT_DIR (configure's test programs): only
//! then may the script's sandbox start programs there (`bstd::procs::hermetic::HermeticOpts::helper_folders`).
//!
//! nm-row: a row's program is on the machine (`path`, its resolved real path) or on the pinned
//! toolchain's shelf (`shelf`, the name of its row in the table's `shelf_programs`, which pins
//! the archive member by digest: [`crate::shelf_pins::shelf_program`]; a tool with no row cannot
//! be granted, shelf-pins) ([`Place`]). `nm` is
//! the shelf's `llvm-nm`, from the same pinned LLVM as its clang and lld, never the machine's
//! `/usr/bin/nm` (an xcrun shim). [`HostTool::programs_in`] names every program at its real
//! path. A row with `env` is named to the script by that variable ([`tool_env`]: `NM`) and its
//! folder is not on `PATH` ([`path_folders`]); a shelf row must have one.
#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};

use blaze_pins::CLANG_PATH;

use crate::toolchain::{object, scalar};

/// The mark's name, as reports and the origin's refusal print it.
pub const MARK: &str = "host-tool";

/// The answer and its command-side grant are paired by the canonical spawn request key.
/// Only a complete, lowercase digest is an identity in this namespace.
pub const SPAWN_ANSWER: &str = "spawn:";
pub const SPAWN_GRANT: &str = "spawn-grant:";

pub fn spawn_receipt(subject: &str) -> Option<(bool, &str)> {
    let (grant, key) = if let Some(key) = subject.strip_prefix(SPAWN_GRANT) {
        (true, key)
    } else {
        (false, subject.strip_prefix(SPAWN_ANSWER)?)
    };
    (key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some((grant, key))
}

/// The kind word a build-script run's read list is filed under (`blaze_record::write_list`).
pub const RUN_KIND: &[u8] = b"build-script run";

/// The kind word of a run that started a host tool: the `host-tool` mark. The origin's sealer
/// refuses a read list filed so (and so the answer only it names).
pub const RUN_KIND_HOST_TOOL: &[u8] = b"build-script run host-tool";

/// The kind word a run's read list is filed under: [`RUN_KIND_HOST_TOOL`] when it was granted a
/// host tool, else [`RUN_KIND`].
pub fn run_kind(host_tools: bool) -> &'static [u8] {
    if host_tools { RUN_KIND_HOST_TOOL } else { RUN_KIND }
}

/// Whether a read list's kind word carries the `host-tool` mark: what the origin's sealer asks.
pub fn marked(kind: &[u8]) -> bool {
    kind == RUN_KIND_HOST_TOOL
}

/// Where a row's own program is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// On the machine: `path` is its resolved real path (`"path"` in the row).
    Machine,
    /// On the pinned toolchain's shelf: `path` is a member of
    /// [`crate::toolchain::SHELF_MEMBERS`] under the shelf (`"shelf"` in the row; nm-row).
    Shelf,
}

/// One tool's row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostTool<'t> {
    pub name: &'t str,
    /// Its own program, granted read and execute: the resolved real path ([`Place::Machine`]) or
    /// the shelf member ([`Place::Shelf`]); [`HostTool::program_in`] names it whole.
    pub path: &'t str,
    pub place: Place,
    /// The variable that names its program to the script (`NM` for nm), set by [`tool_env`];
    /// `None`: found by the `PATH` search, as before. A shelf row always has one.
    pub env: Option<&'t str>,
    /// The folders its runtime reads, space-separated: granted read-only.
    pub reads: &'t str,
    /// The programs the tool itself starts, space-separated real paths, sorted: granted
    /// execute, each keyed by its digest (perl's piped `open` starts `/bin/sh`, which starts
    /// the shell `/private/var/select/sh` names).
    pub runs: &'t str,
    /// The packages whose scripts are known to start it, space-separated, sorted.
    pub needed_by: &'t str,
    /// Whether it starts programs the script wrote under its OUT_DIR (`"runs_out_dir": "yes"`;
    /// D112: sh running configure's test programs): the script granted it may start what is
    /// under its OUT_DIR. Every row says yes or no; nothing is assumed.
    pub runs_out_dir: bool,
}

impl<'t> HostTool<'t> {
    /// The folders its runtime reads.
    pub fn read_folders(&self) -> impl Iterator<Item = &'t str> + use<'t> {
        self.reads.split(' ').filter(|r| !r.is_empty())
    }

    /// The programs it starts.
    pub fn run_paths(&self) -> impl Iterator<Item = &'t str> + use<'t> {
        self.runs.split(' ').filter(|r| !r.is_empty())
    }

    /// Its own program at its real path: `path`, or `<shelf>/<path>` for a shelf row.
    pub fn program_in<'a>(&self, shelf: &str, alloc: Alloc<'a>) -> AString<'a> {
        let mut out: AString<'a> = String::new_in(alloc);
        if self.place == Place::Shelf {
            out.push_str(shelf);
            out.push('/');
        }
        out.push_str(self.path);
        out
    }

    /// Every program granted for it at its real path, its own first ([`HostTool::program_in`]),
    /// then those it starts: what the sandbox may start and the key digests.
    pub fn programs_in<'a>(&self, shelf: &str, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
        let mut out: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        out.push(self.program_in(shelf, alloc));
        for run in self.run_paths() {
            out.push(String::from_str_in(run, alloc));
        }
        out
    }

    /// Whether `package`'s script is known to start it.
    pub fn needed_by(&self, package: &str) -> bool {
        self.needed_by.split(' ').any(|p| p == package)
    }
}

/// A host-tool row that does not say what is asked of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostToolError<'t> {
    /// No `host_tools` row for the target.
    NoRow { target: &'t str },
    /// `names` lists a tool with no object, or a field is missing.
    Malformed { target: &'t str, tool: &'t str, field: &'static str },
    /// `names` is not sorted, or holds a name twice.
    Unsorted { target: &'t str },
}

impl core::fmt::Display for HostToolError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HostToolError::NoRow { target } => write!(f, "{CLANG_PATH}: no `host_tools` row for {target} (D71)"),
            HostToolError::Malformed { target, tool, field } => write!(f, "{CLANG_PATH}: {target}'s host tool `{tool}` has no `{field}` of its form (D71)"),
            HostToolError::Unsorted { target } => write!(f, "{CLANG_PATH}: {target}'s host tool `names` are not sorted and unique (D71)"),
        }
    }
}

/// `target`'s host tools in the table `text`, in `names`' (sorted) order. `names` is the row's
/// last field: the table's reader takes a key's first quoted occurrence, so a one-tool `names`
/// value written before the tool's object would be taken for its key.
pub fn host_tools<'t, 'a>(text: &'t str, target: &'t str, alloc: Alloc<'a>) -> Result<AVec<'a, HostTool<'t>>, HostToolError<'t>> {
    let (rows, _) = object(text, "host_tools", 0).ok_or(HostToolError::NoRow { target })?;
    let (row, _) = object(rows, target, 0).ok_or(HostToolError::NoRow { target })?;
    let names = scalar(row, "names").ok_or(HostToolError::Malformed { target, tool: "", field: "names" })?;
    let mut out: AVec<'a, HostTool<'t>> = Vec::new_in(alloc);
    let mut last: Option<&str> = None;
    for name in names.split(' ').filter(|n| !n.is_empty()) {
        if last.is_some_and(|l| l >= name) {
            return Err(HostToolError::Unsorted { target });
        }
        last = Some(name);
        let malformed = |field| HostToolError::Malformed { target, tool: name, field };
        let (body, _) = object(row, name, 0).ok_or(malformed("object"))?;
        if !body.starts_with('{') {
            return Err(malformed("object"));
        }
        // nm-row: the machine's program by its real path, or a program of the shelf; never both.
        // shelf-pins: the shelf program is named by its pinned row, which gives its shelf path;
        // no row (or a row that is not a program under `bin/`), no grant.
        let (path, place) = match (scalar(body, "path"), scalar(body, "shelf")) {
            (Some(path), None) if path.starts_with('/') => (path, Place::Machine),
            (None, Some(program)) => match crate::shelf_pins::shelf_program(text, target, program) {
                Ok(row) if row.shelf_path.starts_with("bin/") => (row.shelf_path, Place::Shelf),
                _ => return Err(malformed("shelf")),
            },
            (_, Some(_)) => return Err(malformed("shelf")),
            _ => return Err(malformed("path")),
        };
        // The variable naming it: upper case, one row's; a shelf row must have one (the shelf's
        // folder is never on `PATH`).
        let env = scalar(body, "env");
        let env_whole = match env {
            None => place == Place::Machine,
            Some(var) => {
                !var.is_empty()
                    && var.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    && !out.iter().any(|other: &HostTool<'t>| other.env == Some(var))
            }
        };
        if !env_whole {
            return Err(malformed("env"));
        }
        let reads = scalar(body, "reads").ok_or(malformed("reads"))?;
        let runs = scalar(body, "runs").ok_or(malformed("runs"))?;
        if runs.split(' ').filter(|r| !r.is_empty()).any(|r| !r.starts_with('/')) {
            return Err(malformed("runs"));
        }
        let needed_by = scalar(body, "needed_by").ok_or(malformed("needed_by"))?;
        let runs_out_dir = match scalar(body, "runs_out_dir") {
            Some("yes") => true,
            Some("no") => false,
            _ => return Err(malformed("runs_out_dir")),
        };
        out.push(HostTool { name, path, place, env, reads, runs, needed_by, runs_out_dir });
    }
    Ok(out)
}

/// Whether any of `tools` (one script's grant) starts programs the script wrote under its
/// OUT_DIR: then, and only then, the sandbox lets the script start what is under OUT_DIR
/// (D112).
pub fn runs_out_dir(tools: &[HostTool<'_>]) -> bool {
    tools.iter().any(|tool| tool.runs_out_dir)
}

/// The variables that name `tools`' (one script's grant) programs to it, each row's `env` set to
/// its program at its real path under `shelf` (nm-row: `NM=<shelf>/bin/llvm-nm` for
/// tikv-jemalloc-sys's configure), sorted by variable. Rows without `env` give nothing.
pub fn tool_env<'a>(tools: &[HostTool<'_>], shelf: &str, alloc: Alloc<'a>) -> AVec<'a, (AString<'a>, AString<'a>)> {
    let mut out: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(alloc);
    for tool in tools {
        if let Some(var) = tool.env {
            out.push((String::from_str_in(var, alloc), tool.program_in(shelf, alloc)));
        }
    }
    out.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

/// The folders the script's `PATH` holds for `tools` (one script's grant), sorted and unique:
/// the folder of each row found by search (no `env`; sha3-asm starts `Command::new("perl")` and
/// reads no variable). A row named by a variable, and so every shelf row, adds none: nothing
/// else in its folder is found by name.
pub fn path_folders<'t, 'a>(tools: &[HostTool<'t>], alloc: Alloc<'a>) -> AVec<'a, &'t str> {
    let mut out: AVec<'a, &'t str> = Vec::new_in(alloc);
    for tool in tools.iter().filter(|tool| tool.env.is_none()) {
        let (folder, _) = tool.path.rsplit_once('/').unwrap_or(("", tool.path));
        out.push(if folder.is_empty() { "/" } else { folder });
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The tools `package`'s script is granted, from `tools` (one target's rows), in order.
pub fn granted<'t, 'a>(tools: &[HostTool<'t>], package: &str, alloc: Alloc<'a>) -> AVec<'a, HostTool<'t>> {
    let mut out: AVec<'a, HostTool<'t>> = Vec::new_in(alloc);
    for tool in tools {
        if tool.needed_by(package) {
            out.push(*tool);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;

    use super::{HostToolError, MARK, RUN_KIND, RUN_KIND_HOST_TOOL, granted, host_tools, marked, run_kind, runs_out_dir};
    use blaze_pins::CLANG_JSON as PIN;

    /// The checked-in table grants perl, at its real path with the system's Perl folders, to
    /// sha3-asm alone (D71); Linux lists none; a target with no row is refused by name.
    /// Prints "host tools: perl /usr/bin/perl reads /System/Library/Perl /private/var/select/sh
    /// for sha3-asm".
    #[test]
    fn the_pin_table_grants_perl_to_sha3_asm_only() {
        let alloc = Alloc::global();
        let mac = host_tools(PIN, "aarch64-apple-darwin", alloc).expect("the macOS row");
        assert_eq!(mac.iter().map(|t| t.name).collect::<std::vec::Vec<_>>(), ["hostname", "make", "nm", "perl", "sh", "uname"]);
        let perl = *mac.iter().find(|t| t.name == "perl").expect("perl's row");
        assert_eq!((perl.name, perl.path), ("perl", "/usr/bin/perl"));
        assert!(!perl.runs_out_dir);
        assert_eq!(perl.read_folders().collect::<std::vec::Vec<_>>(), ["/System/Library/Perl", "/private/var/select/sh"]);
        assert_eq!(perl.programs_in("/shelf", alloc).iter().map(|p| p.as_str()).collect::<std::vec::Vec<_>>(), ["/usr/bin/perl", "/bin/bash", "/bin/sh"]);
        assert_eq!(granted(&mac, "sha3-asm", alloc).len(), 1);
        assert_eq!(granted(&mac, "zstd-sys", alloc).len(), 0);
        // A prefix of a listed package is not the package.
        assert_eq!(granted(&mac, "sha3", alloc).len(), 0);
        assert_eq!(host_tools(PIN, "aarch64-unknown-linux-gnu", alloc).expect("the Linux row").len(), 0);
        assert_eq!(host_tools(PIN, "x86_64-pc-windows-msvc", alloc).err(), Some(HostToolError::NoRow { target: "x86_64-pc-windows-msvc" }));
        println!("host tools: {} {} reads {} for {}", perl.name, perl.path, perl.reads, perl.needed_by);
    }

    /// The mark: a run granted a tool is filed under its own kind word, which `marked` tells
    /// apart; a plain run's is not marked.
    #[test]
    fn a_host_tool_run_is_filed_marked() {
        assert_eq!(run_kind(true), RUN_KIND_HOST_TOOL);
        assert_eq!(run_kind(false), RUN_KIND);
        assert!(marked(run_kind(true)));
        assert!(!marked(run_kind(false)));
        assert!(std::str::from_utf8(RUN_KIND_HOST_TOOL).expect("text").ends_with(MARK));
    }

    /// Unsorted names are refused, never read in some order.
    #[test]
    fn unsorted_names_are_refused() {
        let text = r#"{"host_tools": {"t": {"names": "perl make", "make": {"path": "/usr/bin/make", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "perl": {"path": "/usr/bin/perl", "reads": "", "runs": "", "needed_by": "y", "runs_out_dir": "no"}}}}"#;
        assert_eq!(host_tools(text, "t", Alloc::global()).err(), Some(HostToolError::Unsorted { target: "t" }));
    }

    /// D112: a row without `runs_out_dir`, or with a value other than yes or no, is refused by
    /// name; nothing is assumed for it. Prints "runs_out_dir refused: missing and maybe".
    #[test]
    fn a_row_must_say_whether_it_runs_out_dir() {
        let alloc = blaze_own::testing::heap().alloc();
        let missing = r#"{"host_tools": {"t": {"sh": {"path": "/bin/sh", "reads": "", "runs": "", "needed_by": "x"}, "names": "sh"}}}"#;
        let maybe = r#"{"host_tools": {"t": {"sh": {"path": "/bin/sh", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "maybe"}, "names": "sh"}}}"#;
        let want = Some(HostToolError::Malformed { target: "t", tool: "sh", field: "runs_out_dir" });
        assert_eq!(host_tools(missing, "t", alloc).err(), want);
        assert_eq!(host_tools(maybe, "t", alloc).err(), want);
        let yes = r#"{"host_tools": {"t": {"sh": {"path": "/bin/sh", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "yes"}, "names": "sh"}}}"#;
        assert!(host_tools(yes, "t", alloc).expect("a whole row")[0].runs_out_dir);
        println!("runs_out_dir refused: missing and maybe");
    }

    /// D112 (main's ruling, wave 3): a row whose grant widens the sandbox (`runs_out_dir` yes:
    /// today sh for tikv-jemalloc-sys, which also lets the script's links use the shelf's
    /// ld64.lld and lld) says in the pin table that it holds only for D112's interim and that
    /// its records stay local. A row given that grant without the word fails here. Prints
    /// "runs_out_dir rows say interim: sh".
    #[test]
    fn a_row_that_runs_out_dir_says_it_is_d112_interim() {
        let alloc = blaze_own::testing::heap().alloc();
        let mac = host_tools(PIN, "aarch64-apple-darwin", alloc).expect("the macOS row");
        let (rows, _) = crate::toolchain::object(PIN, "host_tools", 0).expect("the table");
        let (row, _) = crate::toolchain::object(rows, "aarch64-apple-darwin", 0).expect("the macOS row");
        let mut said: std::vec::Vec<&str> = std::vec::Vec::new();
        for tool in mac.iter().filter(|t| t.runs_out_dir) {
            let (body, _) = crate::toolchain::object(row, tool.name, 0).expect("the tool's row");
            let interim = crate::toolchain::scalar(body, "interim").unwrap_or("");
            assert!(interim.starts_with("D112 interim:") && interim.contains("local"), "{}: a runs_out_dir row says it is D112's interim, records local", tool.name);
            said.push(tool.name);
        }
        assert_eq!(said, ["sh"]);
        println!("runs_out_dir rows say interim: {}", said.join(" "));
    }

    /// D112: tikv-jemalloc-sys's script is granted sh, make, hostname and uname at their real
    /// paths (make the developer folder's real binary, never the `/usr/bin/make` xcrun shim nor
    /// xcrun), sh's runs are sorted and hold the shell and the tools configure starts, and only
    /// sh's row runs what the script wrote under OUT_DIR; sha3-asm keeps perl alone. Prints
    /// "jemalloc tools: hostname make sh uname; runs out dir: sh".
    #[test]
    fn the_pin_table_grants_tikv_jemalloc_sys_its_shell_tools() {
        let alloc = blaze_own::testing::heap().alloc();
        let mac = host_tools(PIN, "aarch64-apple-darwin", alloc).expect("the macOS row");
        let jemalloc = granted(&mac, "tikv-jemalloc-sys", alloc);
        let names: std::vec::Vec<&str> = jemalloc.iter().map(|t| t.name).collect();
        assert_eq!(names, ["hostname", "make", "nm", "sh", "uname"]);
        let row = |name: &str| *jemalloc.iter().find(|t| t.name == name).expect("a granted row");
        assert_eq!(row("sh").path, "/bin/sh");
        assert_eq!(row("make").path, "/Applications/Xcode.app/Contents/Developer/usr/bin/make");
        assert_eq!(row("hostname").path, "/bin/hostname");
        assert_eq!(row("uname").path, "/usr/bin/uname");
        for tool in jemalloc.iter() {
            assert!(
                tool.programs_in("/shelf", alloc).iter().all(|p| p.as_str() != "/usr/bin/make" && !p.as_str().ends_with("/xcrun")),
                "{}: no shim, no xcrun",
                tool.name
            );
        }
        let sh = row("sh");
        let runs: std::vec::Vec<&str> = sh.run_paths().collect();
        let mut sorted = runs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(runs, sorted, "sh's runs are sorted and unique");
        for program in ["/bin/bash", "/bin/mkdir", "/bin/rm", "/usr/bin/sed", "/usr/bin/install", "/usr/bin/awk"] {
            assert!(runs.contains(&program), "sh runs {program}");
        }
        assert!(row("make").run_paths().any(|p| p == "/bin/sh"), "make's recipes run under /bin/sh");
        let out_dir: std::vec::Vec<&str> = jemalloc.iter().filter(|t| t.runs_out_dir).map(|t| t.name).collect();
        assert_eq!(out_dir, ["sh"]);
        assert!(runs_out_dir(&jemalloc));
        let sha3 = granted(&mac, "sha3-asm", alloc);
        assert_eq!(sha3.iter().map(|t| t.name).collect::<std::vec::Vec<_>>(), ["perl"]);
        assert!(!runs_out_dir(&sha3));
        println!("jemalloc tools: {}; runs out dir: {}", names.join(" "), out_dir.join(" "));
    }
}
