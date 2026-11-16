//! What a finding says to a person, in one table: for each rule the tree emits (its area and
//! number), a short stable name (`blaze explain <name>`), the headline and the label beside the
//! marked code, each a template whose `{0}`, `{1}` .. are the finding's arguments. A finding
//! carries its rule and arguments as data; the words are chosen here and nowhere else.

use blaze_own::AString;

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
    },
    Wording {
        area: Area::Cli,
        number: 2,
        name: "dependency-not-read",
        headline: "this build of blaze cannot read the dependency {code:0} yet",
        label: "nothing this package gets from it is checked",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 3,
        name: "manifest-not-read",
        headline: "{0}",
        label: "this build of blaze does not read this yet",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 4,
        name: "module-at-both",
        headline: "the module is at both {code:0} and {code:1}",
        label: "delete one of the two files",
        args: 2,
    },
    Wording {
        area: Area::Cli,
        number: 5,
        name: "unresolved-import",
        headline: "{code:0} is not found: no {code:1} in {code:2}",
        label: "nothing is imported here",
        args: 3,
    },
    Wording {
        area: Area::Cli,
        number: 6,
        name: "private-import",
        headline: "{code:0} is private to its module",
        label: "this module may not name it",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 7,
        name: "ambiguous-import",
        headline: "{code:0} could name two items: two glob imports both bring it",
        label: "name one of them by its full path",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 8,
        name: "std-is-named-std",
        headline: "under edition blaze-2026.11 this crate is named `std`",
        label: "write `std` here",
        args: 0,
    },
    Wording {
        area: Area::Cli,
        number: 9,
        name: "toolchain-crate-named",
        headline: "under edition blaze-2026.11 blaze gives this package {code:0}: this line names another copy",
        label: "delete this line; the crate is `std` with no line",
        args: 1,
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
    },
    Wording {
        area: Area::Cli,
        number: 12,
        name: "source-refused",
        headline: "{0}",
        label: "its bytes are not what `Cargo.lock` pins: fetched again only when the lock changes",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 13,
        name: "cannot-fetch",
        headline: "{0}",
        label: "a blaze built with `fetch` (the default) fetches it",
        args: 1,
    },
    Wording {
        area: Area::Cli,
        number: 14,
        name: "unresolved",
        headline: "no {0} in {1}",
        label: "nothing of this name is here",
        args: 2,
    },
    Wording {
        area: Area::Cli,
        number: 15,
        name: "target-refused",
        headline: "the {0} target {code:1} of package {code:2} is left out: {3}",
        label: "this target is not built, checked or run; fix this table",
        args: 4,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F6,
        name: "too-many-configurations",
        headline: "{0}",
        label: "the configurations multiply past the cap here",
        args: 1,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F7,
        name: "module-file-missing",
        headline: "this module's file is at neither of its places: {code:0} or {code:1}",
        label: "create one of the two files, or delete this line",
        args: 2,
    },
    Wording {
        area: Area::Drive,
        number: 0xC0F8,
        name: "path-file-missing",
        headline: "this module's `#[path]` names {code:0}, which is not in the checkout",
        label: "create the file, or change the path",
        args: 1,
    },
    Wording {
        area: Area::Ladder,
        number: 1,
        name: "not-supported-yet",
        headline: "{0}",
        label: "this build of blaze cannot run this yet",
        args: 1,
    },
    Wording {
        area: Area::Ladder,
        number: 2,
        name: "syntax",
        headline: "{0}",
        label: "the parser stops here",
        args: 1,
    },
    Wording {
        area: Area::Ladder,
        number: 3,
        name: "program-refused",
        headline: "{0}",
        label: "refused here",
        args: 1,
    },
    Wording {
        area: Area::Ladder,
        number: 4,
        name: "unknown-blaze-attribute",
        headline: "{0}",
        label: "inert: nothing in this build reads it",
        args: 1,
    },
    Wording {
        area: Area::Types,
        number: 425,
        name: "E0425",
        headline: "cannot find value {code:0} in this scope",
        label: "not found in this scope",
        args: 1,
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
