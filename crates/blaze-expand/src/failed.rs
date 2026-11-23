//! A failed call's reason, kept past its expansion: the call's error [`Diagnostic`] with the
//! text table its [`Text`]s are ranges of. A [`Diagnostic`] alone names its words only as ranges
//! of [`Expansion::notes`]; a resolver that drops the expansion (a failed call writes nothing)
//! keeps this instead, so the reason it reports later is the matcher's own, typed, never a
//! string made up at the report ("no rule matched; the closest rule expected `$cx:tt :` at
//! `)`").

use core::fmt;

use blaze_own::{Alloc, Bump};

use crate::expansion::{Diagnostic, Expansion, IncludeFailure, ProcLevel, TokenOrigin};
use crate::record::{RecordBytes, RecordColumn, RecordError};
use crate::tokens::Text;

/// Why a call failed: its first error diagnostic ([`Diagnostic::is_error`]), with the texts it
/// names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Failed<'a> {
    pub diagnostic: Diagnostic,
    /// The expansion's note table, copied: what each [`Text`] of `diagnostic` is a range of.
    notes: &'a [u8],
}

impl<'a> Failed<'a> {
    /// The failure of `expansion`: its first error diagnostic, its texts copied into `bump`.
    /// `None` when the expansion has no error (it expanded; a proc macro's warning is no
    /// failure).
    pub fn of(expansion: &Expansion<'_>, bump: Bump<'a>) -> Option<Failed<'a>> {
        let diagnostic = *expansion.diagnostics.iter().find(|d| d.is_error())?;
        Some(Failed { diagnostic, notes: bump.alloc_slice_copy(&expansion.notes) })
    }

    /// A failure from its parts: `notes` must be the table `diagnostic`'s texts are ranges of
    /// (what [`Failed::notes`] gave). For a holder that owns the table (a refusal with no
    /// lifetime keeps `diagnostic` and a copy of `notes`, and shows it through this again).
    pub fn from_parts(diagnostic: Diagnostic, notes: &'a [u8]) -> Failed<'a> {
        Failed { diagnostic, notes }
    }

    /// The note table the diagnostic's texts are ranges of.
    pub fn notes(&self) -> &'a [u8] {
        self.notes
    }

    /// A text the diagnostic names.
    pub fn text(&self, text: Text) -> &'a str {
        text.of(self.notes)
    }

    /// The failure as a record of its own: blaze-expand's expansion record
    /// ([`crate::record::EXPANSION_SCHEMA`], the one encoding of a [`Diagnostic`]) of an
    /// expansion holding this diagnostic and its note table, no tokens. Equal failures write
    /// equal bytes.
    pub fn write_record<'b>(&self, alloc: Alloc<'b>) -> RecordBytes<'b> {
        let mut expansion = Expansion::new_in(alloc);
        expansion.notes.extend_from_slice(self.notes);
        expansion.diagnostics.push(self.diagnostic);
        expansion
            .write_record(&crate::OriginMaps::NONE, alloc)
            .expect("a failure's expansion holds no token, so no origin needs a source")
    }

    /// A failure read back from [`Failed::write_record`]'s bytes (16-aligned, as every
    /// expansion record). A record holding no error diagnostic is refused by name, never read
    /// as "no failure".
    pub fn read_record(bytes: &[u8], bump: Bump<'a>) -> Result<Failed<'a>, RecordError> {
        let expansion = Expansion::read_record(bytes, bump.alloc())?;
        Failed::of(&expansion, bump).ok_or(RecordError::Invalid { column: RecordColumn::Diagnostics, at: 0, what: "a failure's record holds no error diagnostic" })
    }

    /// Where the failure is, when it is a token of the call's input: its index there. The
    /// input of an attribute macro's call is its whole item, so this is a place relative to
    /// the item (rule 71), never to its file.
    pub fn input_token(&self) -> Option<u32> {
        let at = match self.diagnostic {
            Diagnostic::NoRuleMatched { at, .. }
            | Diagnostic::Binding { at, .. }
            | Diagnostic::LeftoverTokens { at }
            | Diagnostic::UnexpectedToken { at }
            | Diagnostic::Builtin { at, .. }
            | Diagnostic::ProcMacro { at, .. } => at,
            Diagnostic::LimitExceeded
            | Diagnostic::BadDefinition { .. }
            | Diagnostic::CompileError { .. }
            | Diagnostic::EnvNotDefined { .. }
            | Diagnostic::Include { .. }
            | Diagnostic::Derive { .. }
            | Diagnostic::ProcMacroPanicked { .. } => None,
        };
        match at {
            Some(TokenOrigin::Input(i)) => Some(i),
            _ => None,
        }
    }
}

/// Where a token origin is, in words.
struct Where(Option<TokenOrigin>);

impl fmt::Display for Where {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => Ok(()),
            Some(TokenOrigin::Input(i)) => write!(f, " (token {i} of the call's input)"),
            Some(TokenOrigin::AttrArgs(i)) => write!(f, " (token {i} of the attribute's arguments)"),
            Some(TokenOrigin::Def(i)) => write!(f, " (token {i} of the macro's definition)"),
            Some(TokenOrigin::Params(i)) => write!(f, " (token {i} of the macro's parameters)"),
            Some(TokenOrigin::Made(_)) => write!(f, " (a token the expansion made)"),
        }
    }
}

impl fmt::Display for Failed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.diagnostic {
            Diagnostic::NoRuleMatched { rules, closest, token, expected, at } => {
                match closest {
                    Some(arm) => write!(f, "no rule of the macro's {rules} matched; the closest, rule {} of {rules}, expected {}", u64::from(arm) + 1, self.text(expected))?,
                    None => write!(f, "no rule of the macro's {rules} matched; the closest expected {}", self.text(expected))?,
                }
                match token {
                    Some(token) => write!(f, " and found `{}`", self.text(token))?,
                    None => write!(f, " and found the end of the input")?,
                }
                write!(f, "{}", Where(at))
            }
            Diagnostic::Binding { message, at } => write!(f, "a rule matched and its transcription failed: {}{}", self.text(message), Where(at)),
            Diagnostic::LeftoverTokens { at } => write!(f, "a rule left input unmatched{}", Where(at)),
            Diagnostic::LimitExceeded => write!(f, "the matcher's bound was hit"),
            Diagnostic::UnexpectedToken { at } => write!(f, "an unexpected token in a rule{}", Where(at)),
            Diagnostic::BadDefinition { message } => write!(f, "the macro's rules do not parse: {}", self.text(message)),
            Diagnostic::CompileError { message } => write!(f, "`compile_error!`: {}", self.text(message)),
            Diagnostic::EnvNotDefined { name, message } => match message {
                Some(message) => write!(f, "`env!(\"{}\")`: {}", self.text(name), self.text(message)),
                None => write!(f, "`env!(\"{}\")`: the variable is not set", self.text(name)),
            },
            Diagnostic::Include { path, why } => {
                let why = match why {
                    IncludeFailure::Missing => "no such file",
                    IncludeFailure::NotUtf8 => "not UTF-8",
                    IncludeFailure::DoesNotLex => "does not lex",
                    IncludeFailure::Outside => "outside the unit's source root",
                };
                write!(f, "including `{}`: {why}", self.text(path))
            }
            Diagnostic::Derive { message } => write!(f, "the derive refused the item: {}", self.text(message)),
            Diagnostic::Builtin { message, at } => write!(f, "{}{}", self.text(message), Where(at)),
            Diagnostic::ProcMacro { level, message, at } => {
                let level = match level {
                    ProcLevel::Error => "error",
                    ProcLevel::Warning => "warning",
                    ProcLevel::Note => "note",
                    ProcLevel::Help => "help",
                };
                write!(f, "the proc macro's {level}: {}{}", self.text(message), Where(at))
            }
            Diagnostic::ProcMacroPanicked { message } => write!(f, "the proc macro panicked: {}", self.text(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    /// A call no rule matches keeps the matcher's words past its expansion: what the closest
    /// rule expected, the token it found, and where in the call's input (rule 19). Expected
    /// line: none printed; the assertion's text is the check.
    #[test]
    fn a_failed_call_keeps_the_matchers_words() {
        let inline = blaze_own::Inline::<4096>::new();
        let erased = blaze_own::Erased::new(&inline);
        let bump = erased.bump();
        let mut expansion = Expansion::new_in(bump.alloc());
        let expected = expansion.note("`$cx:tt :`").expect("a note");
        let token = expansion.note(")").expect("a note");
        expansion.diagnostics.push(Diagnostic::NoRuleMatched { rules: 1, closest: None, token: Some(token), expected, at: Some(TokenOrigin::Input(3)) });
        let failed = Failed::of(&expansion, bump).expect("an error diagnostic is a failure");
        drop(expansion);
        let shown = failed.to_string();
        assert!(shown.contains("the closest expected `$cx:tt :` and found `)`"), "{shown}");
        assert_eq!(failed.input_token(), Some(3));
    }

    /// A failure written as its record reads back equal, with the same words (rule 26: a
    /// restore equals the made value). Expected line: none printed.
    #[test]
    fn a_failure_round_trips_through_its_record() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let inline = blaze_own::Inline::<4096>::new();
        let erased = blaze_own::Erased::new(&inline);
        let bump = erased.bump();
        let mut expansion = Expansion::new_in(alloc);
        let expected = expansion.note("`)`").expect("a note");
        let token = expansion.note("x").expect("a note");
        expansion.diagnostics.push(Diagnostic::NoRuleMatched { rules: 3, closest: Some(1), token: Some(token), expected, at: Some(TokenOrigin::Input(4)) });
        let failed = Failed::of(&expansion, bump).expect("an error diagnostic is a failure");
        let bytes = failed.write_record(alloc);
        // The record is read in place, 16-aligned: copy it to an aligned buffer first.
        let place = blaze_format::Aligned::copy_from(&bytes);
        let back = Failed::read_record(&place, bump).expect("its own record reads back");
        assert_eq!(back, failed);
        assert_eq!(back.to_string(), failed.to_string());
        assert_eq!(failed.write_record(alloc)[..], bytes[..], "equal failures write equal bytes");
    }
}
