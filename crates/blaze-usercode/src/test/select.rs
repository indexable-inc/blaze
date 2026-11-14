//! Which tests a request asks for, as libtest's flags select them ([`Filter`], [`asked`]), what
//! of a test's attributes changes what running it means ([`Attrs`]), and which test binary a
//! test is of ([`Suite`]). Whether an asked test runs, or a kept pass of it stands, is
//! blaze-testsel's machine (docs/v2/plan/design/test-selection.md).

use blaze_ids::{ContentId, Hasher};

use crate::{Given, RunKind};

#[derive(PartialEq, Eq, Hash, Debug)]
pub enum ShouldPanic {
    No,
    Yes,
    WithMessage(String),
}

/// What of a test's attributes changes what running it means.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub ignore: bool,
    pub should_panic: ShouldPanic,
    pub bench: bool,
}

impl Default for Attrs {
    fn default() -> Attrs {
        Attrs { ignore: false, should_panic: ShouldPanic::No, bench: false }
    }
}

impl Attrs {
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze v2 test attributes 1");
        hasher.update(&[u8::from(self.ignore), u8::from(self.bench)]);
        match &self.should_panic {
            ShouldPanic::No => hasher.update(&[0]),
            ShouldPanic::Yes => hasher.update(&[1]),
            ShouldPanic::WithMessage(message) => hasher.update(&[2]).part(message.as_bytes()),
        };
        hasher.finish()
    }
}

/// Which test binary a test is of, without the binary: the same test of another package or
/// target is another test. `context` is the hash of everything outside the reached code that
/// what the binary does depends on (the target triple, the toolchain, the codegen settings
/// and panic strategy), made by whoever builds the binary; a key that misses one is a bug.
#[derive(PartialEq, Eq, Debug)]
pub struct Suite {
    pub package: String,
    /// The target as cargo names it: `lib`, `test:parse`, `bin:blaze`.
    pub target: String,
    pub context: ContentId,
}

impl Suite {
    /// What each test of the suite run in a process of its own is given.
    pub fn given(&self) -> ContentId {
        let mut given = Given::new(RunKind::Test);
        given
            .part("package", self.package.as_bytes())
            .part("target", self.target.as_bytes())
            .id("context", &self.context)
            .part("mode", b"process");
        given.finish()
    }

    /// What each test of the suite run outside the sandbox (`BLAZE_SANDBOX=off`, its package's
    /// `tests = false` or `unsandboxed-tests`, or on macOS no opt-in) is given: [`Suite::given`] and that it runs
    /// outside. A pass is kept only of a sandboxed run, so a test outside is in the index under
    /// a base key of its own, which no kept pass is under: it runs every time, and a pass kept
    /// of the same test inside never stands for it. Its suite's head is still held by
    /// [`Suite::given`].
    pub fn given_outside(&self) -> ContentId {
        let mut given = Given::new(RunKind::Test);
        given
            .part("package", self.package.as_bytes())
            .part("target", self.target.as_bytes())
            .id("context", &self.context)
            .part("mode", b"process")
            .part("sandbox", b"off");
        given.finish()
    }
}

/// Whether libtest's flags ask for a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ignored {
    /// `#[ignore]` tests are reported ignored, not run (the default).
    Skip,
    /// `--ignored`: only they run.
    Only,
    /// `--include-ignored`: they run too.
    Include,
}

/// The names asked for, as libtest takes them.
#[derive(PartialEq, Eq, Debug)]
pub struct Filter {
    /// A test is asked for when its name holds any of these (equals one, with `exact`); all
    /// are when there are none.
    pub names: Vec<String>,
    pub exact: bool,
    /// `--skip`: never a test whose name holds one of these (equals, with `exact`).
    pub skip: Vec<String>,
    pub ignored: Ignored,
}

impl Default for Filter {
    fn default() -> Filter {
        Filter { names: Vec::new(), exact: false, skip: Vec::new(), ignored: Ignored::Skip } // alloc: empty, allocates nothing
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wanted {
    Yes,
    /// Not asked for: not reported.
    Filtered,
    /// Reported ignored, not run.
    Ignored,
}

/// Whether `filter` asks for the test `name` with `attrs`.
pub fn asked(name: &str, attrs: &Attrs, filter: &Filter) -> Wanted {
    let matches = |pattern: &String| match filter.exact {
        true => name == pattern.as_str(),
        false => name.contains(pattern.as_str()),
    };
    if !filter.names.is_empty() && !filter.names.iter().any(matches) {
        return Wanted::Filtered;
    }
    if filter.skip.iter().any(matches) {
        return Wanted::Filtered;
    }
    match (filter.ignored, attrs.ignore) {
        (Ignored::Skip, true) => Wanted::Ignored,
        (Ignored::Only, false) => Wanted::Filtered,
        _ => Wanted::Yes,
    }
}

#[cfg(test)]
mod tests;
