//! `macro_rules!` and `macro`, by mbe's matcher and transcriber driven pure: the definition
//! and the call go in as token trees whose spans say only which token each is
//! ([`crate::bridge`]), and the output's spans come back as origins.
//!
//! mbe takes no database: its matcher and transcriber read only the trees and the spans.

use mbe::{DeclarativeMacro, ExpandErrorKind, MacroCall};
use span::SyntaxContext;

use crate::{
    bridge::{self, Role},
    call::CallKind,
    def::{MacroDefinition, MacroKind},
    error::{ExpandError, Stream},
    expand::Out,
    expansion::Diagnostic,
    tokens::{Transparency, index},
};

/// The definition's rules, parsed.
pub(crate) fn parse(def: &MacroDefinition<'_>) -> Result<DeclarativeMacro, ExpandError> {
    let edition = |ctx: SyntaxContext| ctx.edition();
    let body = bridge::to_tt(def.body(), Role::Def, Stream::Def, def.edition())?;
    Ok(match def.kind() {
        MacroKind::Macro2 => {
            let params = match def.params() {
                Some(params) => {
                    Some(bridge::to_tt(params, Role::Params, Stream::Params, def.edition())?)
                }
                None => None,
            };
            DeclarativeMacro::parse_macro2(params.as_ref(), &body, edition)
        }
        _ => DeclarativeMacro::parse_macro_rules(&body, edition),
    })
}

/// Expands the call in `out` by `rules`.
pub(crate) fn expand(
    rules: &DeclarativeMacro,
    out: &mut Out<'_, '_>,
) -> Result<(), ExpandError> {
    if let Some(error) = rules.err() {
        let message = out.note_display(error)?;
        out.diagnose(Diagnostic::BadDefinition { message });
        return Ok(());
    }
    let call = out.call;
    let site = call.site.edition;
    let input = bridge::to_tt(call.input(), Role::Input, Stream::Input, site)?;
    let args = match call.attr_args() {
        Some(args) => Some(bridge::to_tt(args, Role::AttrArgs, Stream::AttrArgs, site)?),
        None => None,
    };
    let style = match call.kind {
        CallKind::FnLike { .. } => MacroCall::FnLike,
        CallKind::Attr { .. } => MacroCall::Attr { args: args.as_ref() },
        CallKind::Derive { .. } => MacroCall::Derive,
    };
    let call_site = bridge::site_span(site);
    let rule_count = index(rules.num_rules());
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let same = |_: &mut span::Span| {};
        let result = rules.expand_in(frame, &input, same, style, call_site);
        if let Some(error) = &result.err {
            return diagnose(out, error, rule_count);
        }
        let (tree, arm) = &result.value;
        out.exp.arm = *arm;
        bridge::from_tt(out, tree.iter(), Transparency::Transparent)
    })
}

/// mbe's error as the expansion's diagnostic.
fn diagnose(
    out: &mut Out<'_, '_>,
    error: &mbe::ExpandError,
    rules: u32,
) -> Result<(), ExpandError> {
    let (span, kind) = &*error.inner;
    let at = Some(bridge::origin_of(*span, Transparency::Transparent)?);
    let diagnostic = match kind {
        ExpandErrorKind::NoMatchingRule => {
            let expected = out.note("")?;
            Diagnostic::NoRuleMatched { rules, token: None, expected, at }
        }
        ExpandErrorKind::MatchFailure(failure) => {
            let token = match &failure.token {
                Some(token) => Some(out.note(token)?),
                None => None,
            };
            let expected = out.note(&failure.remaining)?;
            Diagnostic::NoRuleMatched { rules, token, expected, at }
        }
        ExpandErrorKind::BindingError(message) | ExpandErrorKind::UnresolvedBinding(message) => {
            Diagnostic::Binding { message: out.note(message)?, at }
        }
        ExpandErrorKind::LeftoverTokens => Diagnostic::LeftoverTokens { at },
        ExpandErrorKind::LimitExceeded => Diagnostic::LimitExceeded,
        ExpandErrorKind::UnexpectedToken => Diagnostic::UnexpectedToken { at },
    };
    out.diagnose(diagnostic);
    Ok(())
}
