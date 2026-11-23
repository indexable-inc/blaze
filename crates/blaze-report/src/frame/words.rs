//! What a finding says to a person, in one table: for each rule the tree emits (its area and
//! number), a short stable name (`blaze explain <name>`), the headline and the label beside the
//! marked code, each a template whose `{0}`, `{1}` .. are the finding's arguments. A finding
//! carries its rule and arguments as data; the words are chosen here and nowhere else.

use blaze_own::AString;

use crate::Severity;

/// Where a rule's number is defined: numbers are unique within an area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Area {
    /// blaze-cli's tasks: the outline's syntax errors and the units reader's findings.
    Cli,
    /// blaze-drive itself (`OVER_CAP_RULE`).
    Drive,
    /// blaze-ladder's refusals (`RefusalRule`).
    Ladder,
    /// The type checker's findings (blaze-typeck `DiagCode`), numbered as rustc's codes.
    Types,
}

/// One rule's words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wording {
    pub area: Area,
    pub number: u16,
    /// Short and stable: shown in the frame's closing corner as `blaze explain <name>`.
    pub name: &'static str,
    /// What a person would say: plain words, code in backticks.
    pub headline: &'static str,
    /// The consequence, beside the marked code.
    pub label: &'static str,
    /// How many arguments the templates take.
    pub args: u8,
    /// What its finding is to the command: how it is drawn and what it makes the exit status
    /// (D164: severity is a column of this table, never an `if` chain of a printer).
    pub kind: Kind,
}

/// What a rule's finding is to the command (D164): its severity under the command's
/// [`Policy`], what it counts as in the exit status ([`crate::query::Counts`]), and whether the
/// default view draws it a line a finding or a line a cause key ([`crate::query::groups`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// An error of the program: drawn as an error; the command fails.
    Error,
    /// Drawn as a warning; fails nothing.
    Warning,
    /// A task that failed on its own: drawn as an error, counted as a failure; the command fails.
    Failure,
    /// An item not checked: a note drawn a finding at a time; the status says items not checked.
    Unchecked,
    /// An item not checked under a cause many items share (a refused macro call, the plan's
    /// refusal of a member's bodies): counted as [`Kind::Unchecked`] is, and drawn by the default
    /// view a line a cause key ([`crate::query::Group::line`]); `--each` draws each.
    Grouped,
    /// A body whose types were checked and whose moves and borrows were not: a note; it fails
    /// nothing and leaves no item unchecked (the borrows stage's coverage says it).
    BorrowsUnchecked,
    /// A move or borrow finding: a note until the borrow check is trusted
    /// ([`Policy::borrows_are_errors`]), an error from then.
    Borrows,
    /// A task that failed only because one it needs failed: a note, counted as a failure under
    /// its cause.
    Waited,
    /// Said, counted as nothing: a view's own line (a group's count), or what the request left
    /// out by the manifest's own rule (a target whose `required-features` are off), not a
    /// check's finding.
    Told,
}

/// What the command decides at view time about a [`Kind`]'s severity: never stored (rule 81).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Whether the move and borrow check is trusted (blaze-cli body_types.rs `BORROWS_ARE_ERRORS`).
    pub borrows_are_errors: bool,
}

impl Kind {
    /// The severity a finding of this kind is drawn with under `policy`.
    pub fn severity(self, policy: Policy) -> Severity {
        match self {
            Kind::Error | Kind::Failure => Severity::Error,
            Kind::Warning => Severity::Warning,
            Kind::Borrows if policy.borrows_are_errors => Severity::Error,
            Kind::Unchecked | Kind::Grouped | Kind::BorrowsUnchecked | Kind::Borrows | Kind::Waited | Kind::Told => Severity::Note,
        }
    }
}

/// One typed argument of a rule's words: words, or a count (drawn in decimal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg<'a> {
    Text(&'a str),
    Count(u64),
}

/// Every rule's words.
pub const WORDINGS: &[Wording] = &[
    Wording {
        area: Area::Cli,
        number: 1,
        name: "syntax",
        headline: "{0}",
        label: "the parser stops here",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 2,
        name: "dependency-not-read",
        headline: "this build of blaze cannot read the dependency {code:0} yet",
        label: "nothing this package gets from it is checked",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 3,
        name: "manifest-not-read",
        headline: "{0}",
        label: "this build of blaze does not read this yet",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 4,
        name: "module-at-both",
        headline: "the module is at both {code:0} and {code:1}",
        label: "delete one of the two files",
        args: 2,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 5,
        name: "unresolved-import",
        headline: "{code:0} is not found: no {code:1} in {code:2}",
        label: "nothing is imported here",
        args: 3,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 6,
        name: "private-import",
        headline: "{code:0} is private to its module",
        label: "this module may not name it",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 7,
        name: "ambiguous-import",
        headline: "{code:0} could name two items: two glob imports both bring it",
        label: "name one of them by its full path",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 8,
        name: "std-is-named-std",
        headline: "under edition blaze-2026.11 this crate is named `std`",
        label: "write `std` here",
        args: 0,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 9,
        name: "toolchain-crate-named",
        headline: "under edition blaze-2026.11 blaze gives this package {code:0}: this line names another copy",
        label: "delete this line; the crate is `std` with no line",
        args: 1,
        kind: Kind::Error,
    },
    // 10, `dependencies-not-read`, retired 2026-11-16: the unit-graph job reads the
    // dependencies. Its number is not reused.
    Wording {
        area: Area::Cli,
        number: 11,
        name: "lock-unread",
        headline: "the workspace's `Cargo.lock` is not read: {0}",
        label: "no dependency's sources are fetched",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 12,
        name: "source-refused",
        headline: "{0}",
        label: "its bytes are not what `Cargo.lock` pins: fetched again only when the lock changes",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 13,
        name: "cannot-fetch",
        headline: "{0}",
        label: "a blaze built with `fetch` (the default) fetches it",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 14,
        name: "unresolved",
        headline: "no {0} in {1}",
        label: "nothing of this name is here",
        args: 2,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 15,
        name: "target-refused",
        headline: "the {0} target {code:1} of package {code:2} is left out: {3}",
        label: "this target is not built, checked or run; fix this table",
        args: 4,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 16,
        name: "task-failed",
        headline: "{0}",
        label: "this is the task's answer until what it read changes: it is kept, never retried",
        args: 1,
        kind: Kind::Failure,
    },
    // A body's type error, from its `BodyCheck` task (blaze-cli body_types.rs): the checker's
    // code and its words.
    Wording {
        area: Area::Cli,
        number: 17,
        name: "types",
        headline: "{1}",
        label: "type error {0}",
        args: 2,
        kind: Kind::Error,
    },
    // A body whose types were not checked to the end (blaze-cli body_types.rs): never a pass;
    // the command's closing line then says "NOT checked: types".
    Wording {
        area: Area::Cli,
        number: 18,
        name: "types-not-checked",
        headline: "item `{0}` was not checked: {1}{2}",
        label: "not a pass: the check stopped here",
        args: 3,
        kind: Kind::Unchecked,
    },
    // A body's move or borrow finding (blaze-cli body_types.rs `BORROWS`): the code and its
    // words. A note until the borrow check is trusted (`BORROWS_ARE_ERRORS`).
    Wording {
        area: Area::Cli,
        number: 19,
        name: "borrows",
        headline: "{1}",
        label: "borrow check {0}",
        args: 2,
        kind: Kind::Borrows,
    },
    // A body whose moves and borrows were not checked to the end (blaze-cli body_types.rs
    // `BORROWS_NOT_CHECKED`): never a pass.
    Wording {
        area: Area::Cli,
        number: 20,
        name: "borrows-not-checked",
        headline: "this body's moves and borrows were not checked: {0}",
        label: "not a pass: the borrow check stopped here",
        args: 1,
        kind: Kind::BorrowsUnchecked,
    },
    // An include read outside its own package, inside the workspace (blaze-cli
    // names/include_outside.rs, decision D111): allowed, a warning.
    Wording {
        area: Area::Cli,
        number: 21,
        name: "include-outside-package",
        headline: "{code:0} reads {code:1}, outside its package {code:2}",
        label: "allowed inside the workspace; a dependency could not read it",
        args: 3,
        kind: Kind::Warning,
    },
    // A macro call of the project's own code whose expansion failed (blaze-cli
    // names/failed_call.rs): the call, then the matcher's reason. An error.
    Wording {
        area: Area::Cli,
        number: 22,
        name: "macro-call-failed",
        headline: "{0} failed: {1}",
        label: "this call wrote nothing: what it would have written is not there",
        args: 2,
        kind: Kind::Error,
    },
    // A task not run because an input it reads could not be made (blaze-cli work.rs
    // `NOT_RUN`): never drawn; print.rs gathers them into one line per cause.
    Wording {
        area: Area::Cli,
        number: 23,
        name: "not-run",
        headline: "{0}",
        label: "not run: an input it reads could not be made",
        args: 1,
        kind: Kind::Unchecked,
    },
    // A task that failed only because a task it needs failed (blaze-cli work.rs `WAITED`): no
    // error of its own. Drawn only under `--each`; else counted under its cause's line
    // (not_checked/waited.rs). `{0}` the cause's name in hex, `{1}` the waiting crate (or empty).
    Wording {
        area: Area::Cli,
        number: 24,
        name: "waited",
        headline: "not checked: it needs {code:0}, which failed (its crate: `{1}`)",
        label: "not checked: a task it needs failed",
        args: 2,
        kind: Kind::Waited,
    },
    // A file whose text this build refuses (blaze-cli work.rs `FILE_NOT_TEXT`): the user's error
    // at that file, never a task failure.
    Wording {
        area: Area::Cli,
        number: 25,
        name: "file-not-text",
        headline: "{0}",
        label: "this file is not text blaze reads (UTF-8, under 4 GiB): fix the file",
        args: 1,
        kind: Kind::Error,
    },
    // A function a test unit's tests record leaves out (blaze-cli names/test_marks.rs): the
    // function, then why. An error at that function; the unit's other tests keep their rows.
    Wording {
        area: Area::Cli,
        number: 26,
        name: "test-refused",
        headline: "{0} is not a test: {1}",
        label: "this function is left out of the tests",
        args: 2,
        kind: Kind::Error,
    },
    // A bare `#[test]` after another test marker on one function (blaze-cli
    // names/test_marks.rs; rustc's `duplicate_macro_attributes`): one test. A warning.
    Wording {
        area: Area::Cli,
        number: 27,
        name: "duplicated-test-attribute",
        headline: "{0} is marked as a test more than once",
        label: "duplicated attribute: it is one test; remove one marker",
        args: 1,
        kind: Kind::Warning,
    },
    Wording {
        area: Area::Cli,
        number: 28,
        name: "unresolved-private",
        headline: "{code:0} is private in this scope",
        label: "this module may not name {code:0}",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 29,
        name: "unresolved-ambiguous",
        headline: "{code:0} is ambiguous ({1})",
        label: "this name has more than one binding",
        args: 2,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 30,
        name: "unresolved-not-a-module",
        headline: "{code:0} is neither a module nor an enum",
        label: "the path cannot continue through {code:0}",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 31,
        name: "unresolved-super-above-root",
        headline: "`super` goes above the crate root in {code:0}",
        label: "the crate root has no parent module",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 32,
        name: "unresolved-dollar-crate",
        headline: "the crate named by `$crate` in {code:0} is not a dependency",
        label: "`$crate` names the crate that defined the macro",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 33,
        name: "unresolved-wrong-macro-kind",
        headline: "{code:0} names a different kind of macro than this call requires",
        label: "{code:0} cannot be called in this form",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Cli,
        number: 34,
        name: "unresolved-derive-after-attribute",
        headline: "attribute macro {code:0} was refused because a `#[derive]` is written after it on the same item",
        label: "move the derive before {code:0} or into its output",
        args: 1,
        kind: Kind::Error,
    },
    // A body's note (D164): a finding whose subject is the body's item (`{0}` its file, `{1}`
    // its item: crate::finding::Subject::Item) and whose cause (crate::finding::Cause) gives the
    // rest. Each row is its cause's (`CauseKind::rule`); the words are those blaze-cli
    // engine_bodies.rs `note` built in place before the switch, byte for byte
    // (crate::finding::tests `the_table_says_what_the_old_sentences_said`).
    Wording {
        area: Area::Cli,
        number: 35,
        name: "body-task-fault",
        headline: "{code:0}: the body of {code:1} was not checked: {2} (engine_bodies.rs)",
        label: "the plan or the view refused the body's task: a fault of blaze's",
        args: 3,
        kind: Kind::Failure,
    },
    Wording {
        area: Area::Cli,
        number: 36,
        name: "body-check-panicked",
        headline: "{code:0}: the check of the body of {code:1} panicked (Panicked); that body alone failed (engine_bodies.rs)",
        label: "not checked: its check panicked",
        args: 2,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 37,
        name: "body-stretch-filled",
        headline: "{code:0}: the shared stretch of {2} bytes filled while checking {code:1} (FrameUsedUp); the body is owed because its available space depends on the stretch (engine_bodies.rs)",
        label: "not checked this request: owed to the next",
        args: 3,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 38,
        name: "body-frame-used-up",
        headline: "{code:0}: the body of {code:1} used up its frame (FrameUsedUp) for {2}, limit {3} bytes; that body alone failed (engine_bodies.rs)",
        label: "not checked: its frame is used up",
        args: 4,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 39,
        name: "body-refused",
        headline: "{code:0}: the body of {code:1} was not checked: {2} (engine_bodies.rs)",
        label: "not checked: this stage could not give it a check",
        args: 3,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 40,
        name: "body-refused-at-plan",
        headline: "{code:0}: body {code:1}: not checked because {2}",
        label: "not checked: the plan refused it by its file's, member's or unit's records",
        args: 3,
        kind: Kind::Grouped,
    },
    Wording {
        area: Area::Cli,
        number: 41,
        name: "body-verdict-silent",
        headline: "{code:0}: body {code:1}: not checked whole: its verdict failed and said no cause (front2 tables/task.rs `said_of`; engine_bodies.rs)",
        label: "not checked whole: no cause was said",
        args: 2,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 42,
        name: "body-not-whole",
        headline: "{code:0}: body {code:1}: {2}",
        label: "not checked whole: its verdict said why",
        args: 3,
        kind: Kind::Unchecked,
    },
    Wording {
        area: Area::Cli,
        number: 43,
        name: "body-under-refused-call",
        headline: "{code:0}: body {code:1}: not checked because {2}",
        label: "not checked: it reads a name a refused macro call may have made",
        args: 3,
        kind: Kind::Grouped,
    },
    Wording {
        area: Area::Cli,
        number: 44,
        name: "body-borrows-refused",
        headline: "{code:0}: body {code:1}: types checked; {2}",
        label: "its moves and borrows were not checked",
        args: 3,
        kind: Kind::BorrowsUnchecked,
    },
    // The default view's line for one cause key (crate::query::Group::line): `{0}` how many
    // subjects, `{1}` their noun, `{2}` the cause's words, `{3}` the first subjects named.
    Wording {
        area: Area::Cli,
        number: 45,
        name: "not-checked-group",
        headline: "{0} {1} not checked because {2}: {3}",
        label: "one line for every item under one cause; `--each` draws each",
        args: 4,
        kind: Kind::Told,
    },
    // A root's target the unit graph leaves out by its `required-features` (blaze-unit-graph
    // `LeftOut`, said by blaze-cli graph.rs): the whole sentence, naming the target and what is off.
    Wording {
        area: Area::Cli,
        number: 46,
        name: "target-skipped",
        headline: "{0}",
        label: "not built, checked or run until those features are on (`--features`, `--all-features`)",
        args: 1,
        kind: Kind::Told,
    },
    // The rustflags (cargo's `build.rustflags` rule) a unit takes that blaze does not apply: each
    // `--cfg` is in the units' cfg, every other flag is named here, never dropped quietly
    // (blaze-cli graph.rs `RUSTFLAGS_NOT_APPLIED`, blaze-manifest rustflags.rs `Chosen`).
    Wording {
        area: Area::Cli,
        number: 47,
        name: "rustflags-not-applied",
        headline: "{0}",
        label: "blaze applies a rustflags `--cfg` and no other flag",
        args: 1,
        kind: Kind::Told,
    },
    // A dependency declared in a member's `Cargo.toml` that none of that member's checked targets
    // reference (blaze-cli unused_deps.rs `UNUSED_DEPENDENCY`).
    Wording {
        area: Area::Cli,
        number: 48,
        name: "unused-dependency",
        headline: "dependency {code:0} is declared in {code:1} of {code:2} but never referenced",
        label: "unused dependency; remove this line",
        args: 3,
        kind: Kind::Warning,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F6,
        name: "too-many-configurations",
        headline: "{0}",
        label: "the configurations multiply past the cap here",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F7,
        name: "module-file-missing",
        headline: "this module's file is at neither of its places: {code:0} or {code:1}",
        label: "create one of the two files, or delete this line",
        args: 2,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F8,
        name: "path-file-missing",
        headline: "this module's `#[path]` names {code:0}, which is not in the checkout",
        label: "create the file, or change the path",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Ladder,
        number: 1,
        name: "not-supported-yet",
        headline: "{0}",
        label: "this build of blaze cannot run this yet",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Ladder,
        number: 2,
        name: "syntax",
        headline: "{0}",
        label: "the parser stops here",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Ladder,
        number: 3,
        name: "program-refused",
        headline: "{0}",
        label: "refused here",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Ladder,
        number: 4,
        name: "unknown-blaze-attribute",
        headline: "{0}",
        label: "inert: nothing in this build reads it",
        args: 1,
        kind: Kind::Error,
    },
    Wording {
        area: Area::Types,
        number: 425,
        name: "E0425",
        headline: "cannot find value {code:0} in this scope",
        label: "not found in this scope",
        args: 1,
        kind: Kind::Error,
    },
];

/// What a help line says about a name that did not resolve, by what the fix leaves wrong
/// (blaze-typeck `suggest::Pick`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpKind {
    /// The one candidate of the right type that nothing uses: `{0}` its name, `{1}` its type.
    NeverUsed,
    /// The one candidate spelled almost the same: `{0}` its name.
    SpelledAlike,
    /// Two candidates equal on every count, no fix: `{0}` and `{1}` their names.
    Tied,
    /// A manifest line naming the toolchain's crate again, deleted: `{0}` the name it gave.
    ToolchainLine,
    /// A mechanical fix from a lint rule: `{0}` names the rule.
    LintFix,
    /// A manifest line naming an unused dependency, deleted: `{0}` the dependency key.
    UnusedDependencyLine,
}

/// One help's words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HelpWording {
    pub kind: HelpKind,
    pub name: &'static str,
    pub template: &'static str,
    pub args: u8,
}

/// Every help's words.
pub const HELPS: &[HelpWording] = &[
    HelpWording {
        kind: HelpKind::NeverUsed,
        name: "help-never-used",
        template: "{code:0} is in scope, is a {code:1}, and is never used",
        args: 2,
    },
    HelpWording {
        kind: HelpKind::SpelledAlike,
        name: "help-spelled-alike",
        template: "{code:0} is in scope and is spelled almost the same",
        args: 1,
    },
    HelpWording {
        kind: HelpKind::Tied,
        name: "help-tied",
        template: "{code:0} and {code:1} are both in scope and fit equally well: no fix is offered",
        args: 2,
    },
    HelpWording {
        kind: HelpKind::ToolchainLine,
        name: "help-toolchain-line",
        template: "under edition blaze-2026.11 the package is given {code:0} with no line",
        args: 1,
    },
    HelpWording {
        kind: HelpKind::LintFix,
        name: "help-lint-fix",
        template: "mechanical fix for lint {code:0}",
        args: 1,
    },
    HelpWording {
        kind: HelpKind::UnusedDependencyLine,
        name: "help-unused-dependency-line",
        template: "remove unused dependency {code:0}",
        args: 1,
    },
];

/// The words of help `kind` filled from `args` into `out`.
pub fn help_words(out: &mut AString<'_>, kind: HelpKind, args: &[&str]) -> Result<(), WordsError> {
    let Some(w) = HELPS.iter().find(|w| w.kind == kind) else {
        return Err(WordsError::Arguments { name: "help", given: args.len(), takes: 0 });
    };
    if args.len() != usize::from(w.args) {
        return Err(WordsError::Arguments { name: w.name, given: args.len(), takes: w.args });
    }
    fill(out, w.template, args);
    Ok(())
}

/// The words of `area`'s rule `number`.
pub fn wording(area: Area, number: u16) -> Option<&'static Wording> {
    WORDINGS.iter().find(|it| it.area == area && it.number == number)
}

/// Why a template was not filled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WordsError {
    /// The rule has no wording: a row of [`WORDINGS`] is missing.
    NoWording { area: Area, number: u16 },
    /// The finding gave `given` arguments and the rule's words take `takes`.
    Arguments { name: &'static str, given: usize, takes: u8 },
}

impl core::fmt::Display for WordsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            WordsError::NoWording { area, number } => {
                write!(f, "blaze_report::frame::words: rule {number} of {area:?} has no wording in WORDINGS")
            }
            WordsError::Arguments { name, given, takes } => {
                write!(f, "blaze_report::frame::words: rule {name} takes {takes} arguments and was given {given}")
            }
        }
    }
}

impl core::error::Error for WordsError {}

/// A finding's words: its rule's name, and the headline and label filled from `args` into
/// `headline` and `label`.
pub fn say(
    area: Area,
    number: u16,
    args: &[&str],
    headline: &mut AString<'_>,
    label: &mut AString<'_>,
) -> Result<&'static str, WordsError> {
    let wording = wording(area, number).ok_or(WordsError::NoWording { area, number })?;
    if args.len() != usize::from(wording.args) {
        return Err(WordsError::Arguments { name: wording.name, given: args.len(), takes: wording.args });
    }
    fill(headline, wording.headline, args);
    fill(label, wording.label, args);
    Ok(wording.name)
}

/// `template` with each `{n}` the `n`th of `args` as words and each `{code:n}` as code (set
/// in backticks, which the frame draws as code); any other `{` is itself.
pub fn fill(out: &mut AString<'_>, template: &str, args: &[&str]) {
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let (code, body) = match after.strip_prefix("code:") {
            Some(body) => (true, body),
            None => (false, after),
        };
        let digit = body.as_bytes().first().filter(|b| b.is_ascii_digit());
        match (digit, body.as_bytes().get(1)) {
            (Some(&digit), Some(b'}')) if usize::from(digit - b'0') < args.len() => {
                if code {
                    out.push('`');
                }
                out.push_str(args[usize::from(digit - b'0')]);
                if code {
                    out.push('`');
                }
                rest = &body[2..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

/// [`fill`] over typed arguments into any writer: a [`Arg::Count`] is drawn in decimal, a
/// `{code:n}` set in backticks. The one filler of a finding's words ([`crate::finding`]).
pub fn fill_args(out: &mut dyn core::fmt::Write, template: &str, args: &[Arg<'_>]) -> core::fmt::Result {
    fill_with(out, template, args.len(), &mut |n: usize, out: &mut dyn core::fmt::Write| match args[n] {
        Arg::Text(text) => out.write_str(text),
        Arg::Count(count) => write!(out, "{count}"),
    })
}

/// [`fill_args`] with each of the `args` arguments written by `arg` (its number, the writer):
/// for an argument that is itself drawn from parts (a group's list of subjects,
/// [`crate::query::Group::line`]), written in place with nothing kept between.
pub fn fill_with(
    out: &mut dyn core::fmt::Write,
    template: &str,
    args: usize,
    arg: &mut dyn FnMut(usize, &mut dyn core::fmt::Write) -> core::fmt::Result,
) -> core::fmt::Result {
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.write_str(&rest[..open])?;
        let after = &rest[open + 1..];
        let (code, body) = match after.strip_prefix("code:") {
            Some(body) => (true, body),
            None => (false, after),
        };
        let digit = body.as_bytes().first().filter(|b| b.is_ascii_digit());
        match (digit, body.as_bytes().get(1)) {
            (Some(&digit), Some(b'}')) if usize::from(digit - b'0') < args => {
                if code {
                    out.write_char('`')?;
                }
                arg(usize::from(digit - b'0'), out)?;
                if code {
                    out.write_char('`')?;
                }
                rest = &body[2..];
            }
            _ => {
                out.write_char('{')?;
                rest = after;
            }
        }
    }
    out.write_str(rest)
}
