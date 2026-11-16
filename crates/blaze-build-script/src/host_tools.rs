//! D71: the host tools a native build script may start besides the pinned C toolchain (today:
//! `perl`, which sha3-asm starts to turn cryptogams' perlasm into assembly). The list's one home
//! is the pin table's `host_tools` (`crates/hir-ty-codegen/clang.json`, [`crate::toolchain::PIN`]).
//!
//! A script whose package is in a tool's `needed_by` is granted read and execute of the tool's
//! resolved real path and read-only of the folders its runtime needs; the tool's binary digest
//! is in its run key ([`crate::key::HostToolKey`]); its record is filed under
//! [`RUN_KIND_HOST_TOOL`], the `host-tool` mark, so it stays on this machine: the shared-cache
//! origin refuses to seal a record so marked. A tool not on the list is refused by the sandbox
//! as before (only granted programs may be started). Which tools a script gets is decided from
//! text (the table and the package's name) before the run; nothing searches `PATH` for it.
//! Sans-IO: the caller resolves and digests the path.
#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

use crate::toolchain::{PIN_PATH, object, scalar};

/// The mark's name, as reports and the origin's refusal print it.
pub const MARK: &str = "host-tool";

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

/// One tool's row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostTool<'t> {
    pub name: &'t str,
    /// The resolved real path: granted read and execute.
    pub path: &'t str,
    /// The folders its runtime reads, space-separated: granted read-only.
    pub reads: &'t str,
    /// The programs the tool itself starts, space-separated real paths, sorted: granted
    /// execute, each keyed by its digest (perl's piped `open` starts `/bin/sh`, which starts
    /// the shell `/private/var/select/sh` names).
    pub runs: &'t str,
    /// The packages whose scripts are known to start it, space-separated, sorted.
    pub needed_by: &'t str,
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

    /// Every program granted for it, its own path first: what the sandbox may start and the
    /// key digests.
    pub fn programs(&self) -> impl Iterator<Item = &'t str> + use<'t> {
        core::iter::once(self.path).chain(self.run_paths())
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
            HostToolError::NoRow { target } => write!(f, "{PIN_PATH}: no `host_tools` row for {target} (D71)"),
            HostToolError::Malformed { target, tool, field } => write!(f, "{PIN_PATH}: {target}'s host tool `{tool}` has no `{field}` of its form (D71)"),
            HostToolError::Unsorted { target } => write!(f, "{PIN_PATH}: {target}'s host tool `names` are not sorted and unique (D71)"),
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
        let path = scalar(body, "path").filter(|p| p.starts_with('/')).ok_or(malformed("path"))?;
        let reads = scalar(body, "reads").ok_or(malformed("reads"))?;
        let runs = scalar(body, "runs").ok_or(malformed("runs"))?;
        if runs.split(' ').filter(|r| !r.is_empty()).any(|r| !r.starts_with('/')) {
            return Err(malformed("runs"));
        }
        let needed_by = scalar(body, "needed_by").ok_or(malformed("needed_by"))?;
        out.push(HostTool { name, path, reads, runs, needed_by });
    }
    Ok(out)
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

    use super::{HostToolError, MARK, RUN_KIND, RUN_KIND_HOST_TOOL, granted, host_tools, marked, run_kind};
    use crate::toolchain::PIN;

    /// The checked-in table grants perl, at its real path with the system's Perl folders, to
    /// sha3-asm alone (D71); Linux lists none; a target with no row is refused by name.
    /// Prints "host tools: perl /usr/bin/perl reads /System/Library/Perl /private/var/select/sh
    /// for sha3-asm".
    #[test]
    fn the_pin_table_grants_perl_to_sha3_asm_only() {
        let alloc = Alloc::global();
        let mac = host_tools(PIN, "aarch64-apple-darwin", alloc).expect("the macOS row");
        assert_eq!(mac.len(), 1);
        let perl = mac[0];
        assert_eq!((perl.name, perl.path), ("perl", "/usr/bin/perl"));
        assert_eq!(perl.read_folders().collect::<std::vec::Vec<_>>(), ["/System/Library/Perl", "/private/var/select/sh"]);
        assert_eq!(perl.programs().collect::<std::vec::Vec<_>>(), ["/usr/bin/perl", "/bin/bash", "/bin/sh"]);
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
        let text = r#"{"host_tools": {"t": {"names": "perl make", "make": {"path": "/usr/bin/make", "reads": "", "runs": "", "needed_by": "x"}, "perl": {"path": "/usr/bin/perl", "reads": "", "runs": "", "needed_by": "y"}}}}"#;
        assert_eq!(host_tools(text, "t", Alloc::global()).err(), Some(HostToolError::Unsorted { target: "t" }));
    }
}
