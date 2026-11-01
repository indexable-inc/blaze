//! `meta_variable_misuse`: the uses of a rule's variables in its transcriber that cannot
//! expand (one unknown, one still repeating, one repeating with another operator than where it
//! is bound) and a variable bound twice in a matcher of a macro a transcriber defines; the
//! binders followed into the `macro_rules!` and `macro` definitions a transcriber writes.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_check.rs` (`check_meta_variables`,
//! `check_binders`, `check_occurrences`, `check_nested_occurrences`, `check_nested_macro`,
//! `check_ops_is_prefix`, `ops_is_prefix`) and its diagnostics in
//! `compiler/rustc_expand/src/diagnostics.rs` (MIT or Apache-2.0).

use blaze_hash::HashMap;
use smallvec::SmallVec;
use syntax::TextRange;

use super::{
    quoted::{Kleene, Matchers, Mtt},
    tokens::{Delim, Ids, Kind, Punct, Token, Tokens},
};

/// What `meta_variable_misuse` says, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Misuse {
    /// `DuplicateMatcherBindingLint`: `span` binds again what `prev` bound, in a matcher of a
    /// macro a transcriber defines.
    Duplicate { span: TextRange, prev: TextRange },
    /// `UnknownMacroVariable`.
    Unknown { span: TextRange, name: Token },
    /// `MetaVarStillRepeatingLint`: `binder` is the operator of the repetition it is bound in.
    StillRepeating { span: TextRange, binder: TextRange, name: Token },
    /// `MetaVariableWrongOperator`: the operators of the binder's repetition and of the use's.
    WrongOperator { span: TextRange, binder: TextRange, occurrence: TextRange },
}

/// What a rule's matcher binds: rustc's `BinderInfo`.
struct BinderInfo {
    span: TextRange,
    /// The operators of the repetitions it is bound in, outermost first.
    ops: SmallVec<[Kleene; 1]>,
}

type Binders<'t> = HashMap<&'t str, BinderInfo>;

/// Where a transcriber defines a macro: the binders of the rule it is in and the operators
/// of the repetitions it is in (rustc's `MacroState`).
struct MacroState<'a, 't> {
    binders: &'a Binders<'t>,
    ops: SmallVec<[Kleene; 1]>,
}

/// rustc's `Stack`: a list on the call stack, innermost on top.
enum Stack<'a, T> {
    Empty,
    Push { top: T, prev: &'a Stack<'a, T> },
}

impl<'a, T> Stack<'a, T> {
    fn is_empty(&self) -> bool {
        matches!(self, Stack::Empty)
    }

    fn push(&'a self, top: T) -> Stack<'a, T> {
        Stack::Push { top, prev: self }
    }

    /// From the top down.
    fn iter(&'a self) -> impl Iterator<Item = &'a T> {
        let mut at: &'a Stack<'a, T> = self;
        std::iter::from_fn(move || match *at {
            Stack::Empty => None,
            Stack::Push { ref top, prev } => {
                at = prev;
                Some(top)
            }
        })
    }
}

impl Stack<'_, Kleene> {
    /// Outermost first.
    fn ops(&self) -> SmallVec<[Kleene; 1]> {
        // alloc: inline, a `SmallVec` of one allocates nothing below two levels of repetition
        let mut ops: SmallVec<[Kleene; 1]> = self.iter().copied().collect();
        ops.reverse();
        ops
    }
}

/// The checks of one definition's rules.
pub(super) struct Checker<'t> {
    m: &'t Matchers,
    tokens: &'t Tokens<'t>,
    pub(super) misuses: Vec<Misuse>,
}

impl<'t> Checker<'t> {
    pub(super) fn new(m: &'t Matchers, tokens: &'t Tokens<'t>) -> Checker<'t> {
        // alloc: empty until a lint fires; holds only what fires
        Checker { m, tokens, misuses: Vec::new() }
    }

    fn name(&self, name: Token) -> &'t str {
        self.tokens.name(name.range)
    }

    /// rustc's `check_meta_variables`: one rule, its `attr(..)` arguments bound first.
    pub(super) fn rule(&mut self, args: Option<u32>, lhs: u32, rhs: u32) {
        let mut binders = Binders::default();
        if let Some(args) = args {
            self.binders(args, &Stack::Empty, &mut binders, &Stack::Empty);
        }
        self.binders(lhs, &Stack::Empty, &mut binders, &Stack::Empty);
        self.occurrences(rhs, &Stack::Empty, &binders, &Stack::Empty);
    }

    /// rustc's `check_binders`: what the matcher `lhs` binds, under the outer macros
    /// `macros` and the repetitions `ops`.
    fn binders(
        &mut self,
        lhs: u32,
        macros: &Stack<'_, MacroState<'_, 't>>,
        binders: &mut Binders<'t>,
        ops: &Stack<'_, Kleene>,
    ) {
        let m = self.m;
        match *m.at(lhs) {
            Mtt::Token(_) | Mtt::MetaVarExpr { .. } => {}
            // In a matcher a transcriber writes, `$x:frag` is `$x`, `:` and `frag`.
            Mtt::MetaVar { range, name } => {
                let name = self.name(name);
                if let Some(prev) = binders.get(name) {
                    self.misuses.push(Misuse::Duplicate { span: range, prev: prev.span });
                } else if binder_info(macros, binders, name).is_none() {
                    binders.insert(name, BinderInfo { span: range, ops: ops.ops() });
                } else {
                    self.occurrences(lhs, macros, binders, ops);
                }
            }
            Mtt::MetaVarDecl { range, name, .. } => {
                // Bound again it is rustc's `DuplicateMatcherBinding`, an error, not a lint.
                let name = self.name(name);
                if binder_info(macros, binders, name).is_none() {
                    binders.insert(name, BinderInfo { span: range, ops: ops.ops() });
                }
            }
            Mtt::Delimited { tts, .. } => {
                for tt in tts.iter() {
                    self.binders(tt, macros, binders, ops);
                }
            }
            Mtt::Sequence { tts, kleene, .. } => {
                let ops = ops.push(kleene);
                for tt in tts.iter() {
                    self.binders(tt, macros, binders, &ops);
                }
            }
        }
    }

    /// rustc's `check_occurrences`: the uses in the transcriber `rhs`.
    fn occurrences(
        &mut self,
        rhs: u32,
        macros: &Stack<'_, MacroState<'_, 't>>,
        binders: &Binders<'t>,
        ops: &Stack<'_, Kleene>,
    ) {
        let m = self.m;
        match *m.at(rhs) {
            Mtt::Token(_) | Mtt::MetaVarDecl { .. } => {}
            Mtt::MetaVar { range, name } => self.ops_is_prefix_of(macros, binders, ops, range, name),
            Mtt::MetaVarExpr { range, vars } => {
                for &var in &m.vars[vars.start as usize..vars.end as usize] {
                    self.ops_is_prefix_of(macros, binders, ops, range, var);
                }
            }
            Mtt::Delimited { tts, .. } => self.nested_occurrences(tts, macros, binders, ops),
            Mtt::Sequence { tts, kleene, .. } => {
                let ops = ops.push(kleene);
                self.nested_occurrences(tts, macros, binders, &ops);
            }
        }
    }

    /// rustc's `check_nested_occurrences`: the uses in `tts`, and the definitions of macros in
    /// them (`macro_rules! name { .. }`, `macro name { .. }`, `macro name(..) { .. }`).
    fn nested_occurrences(
        &mut self,
        tts: Ids,
        macros: &Stack<'_, MacroState<'_, 't>>,
        binders: &Binders<'t>,
        ops: &Stack<'_, Kleene>,
    ) {
        let (m, tokens) = (self.m, self.tokens);
        let mut state = Nested::Empty;
        let nested_macros = macros.push(MacroState { binders, ops: ops.ops() });
        let mut nested_binders = Binders::default();
        for tt in tts.iter() {
            match (state, *m.at(tt)) {
                (Nested::Empty, Mtt::Token(token)) if token.kind == (Kind::Ident { raw: false }) => {
                    match tokens.text(token.range) {
                        "macro_rules" => state = Nested::MacroRules,
                        "macro" => state = Nested::Macro,
                        _ => {}
                    }
                }
                (Nested::MacroRules, Mtt::Token(token)) if token.is(Punct::Bang) => state = Nested::MacroRulesBang,
                (Nested::MacroRulesBang, Mtt::Token(token)) if matches!(token.kind, Kind::Ident { .. }) => {
                    state = Nested::MacroRulesBangName;
                }
                (Nested::MacroRulesBang, Mtt::MetaVar { .. }) => {
                    state = Nested::MacroRulesBangName;
                    self.occurrences(tt, macros, binders, ops);
                }
                (Nested::MacroRulesBangName | Nested::MacroName, Mtt::Delimited { delim: Delim::Brace, tts: body, .. }) => {
                    let macro_rules = state == Nested::MacroRulesBangName;
                    state = Nested::Empty;
                    let rest = self.nested_macro(macro_rules, body, &nested_macros);
                    self.nested_occurrences(body.skip(rest), macros, binders, ops);
                }
                (Nested::Macro, Mtt::Token(token)) if matches!(token.kind, Kind::Ident { .. }) => {
                    state = Nested::MacroName;
                }
                (Nested::Macro, Mtt::MetaVar { .. }) => {
                    state = Nested::MacroName;
                    self.occurrences(tt, macros, binders, ops);
                }
                (Nested::MacroName, Mtt::Delimited { delim: Delim::Paren, .. }) => {
                    state = Nested::MacroNameParen;
                    nested_binders = Binders::default();
                    self.binders(tt, &nested_macros, &mut nested_binders, &Stack::Empty);
                }
                (Nested::MacroNameParen, Mtt::Delimited { delim: Delim::Brace, .. }) => {
                    state = Nested::Empty;
                    self.occurrences(tt, &nested_macros, &nested_binders, &Stack::Empty);
                }
                _ => {
                    state = Nested::Empty;
                    self.occurrences(tt, macros, binders, ops);
                }
            }
        }
    }

    /// rustc's `check_nested_macro`: the rules `(lhs) => {rhs}` of a macro a transcriber
    /// defines, as long as `tts` reads as them; how many token trees were read.
    fn nested_macro(&mut self, macro_rules: bool, tts: Ids, macros: &Stack<'_, MacroState<'_, 't>>) -> usize {
        let m = self.m;
        let n = tts.len();
        let separator = if macro_rules { Punct::Semi } else { Punct::Comma };
        let at = |i: usize| m.at(tts.start + i as u32);
        let is_delimited = |tt: &Mtt| matches!(tt, Mtt::Delimited { .. });
        let is = |tt: &Mtt, punct| matches!(tt, Mtt::Token(it) if it.is(punct));
        let mut i = 0;
        loop {
            if i + 2 >= n || !is_delimited(at(i)) || !is(at(i + 1), Punct::FatArrow) || !is_delimited(at(i + 2)) {
                break;
            }
            let (lhs, rhs) = (tts.start + i as u32, tts.start + i as u32 + 2);
            let mut binders = Binders::default();
            self.binders(lhs, macros, &mut binders, &Stack::Empty);
            self.occurrences(rhs, macros, &binders, &Stack::Empty);
            i += 3;
            if i == n || !is(at(i), separator) {
                break;
            }
            i += 1;
        }
        i
    }

    /// rustc's `check_ops_is_prefix`: that the use `span` of `name` is under the repetitions
    /// its binder is under, found through the macros it is nested in.
    fn ops_is_prefix_of(
        &mut self,
        macros: &Stack<'_, MacroState<'_, 't>>,
        binders: &Binders<'t>,
        ops: &Stack<'_, Kleene>,
        span: TextRange,
        name: Token,
    ) {
        let key = self.name(name);
        let macros = macros.push(MacroState { binders, ops: ops.ops() });
        // The operators of each state up to the one that binds it, innermost first.
        let mut acc: SmallVec<[&SmallVec<[Kleene; 1]>; 1]> = SmallVec::new();
        for state in macros.iter() {
            acc.push(&state.ops);
            if let Some(binder) = state.binders.get(key) {
                // alloc: inline, a `SmallVec` of two allocates nothing below three levels of repetition
                let occurrence: SmallVec<[Kleene; 2]> = acc.iter().rev().flat_map(|it| it.iter().copied()).collect();
                if let Some(misuse) = ops_is_prefix(span, name, &binder.ops, &occurrence) {
                    self.misuses.push(misuse);
                }
                return;
            }
        }
        self.misuses.push(Misuse::Unknown { span, name });
    }
}

/// rustc's `get_binder_info`.
fn binder_info<'a, 't>(
    macros: &'a Stack<'a, MacroState<'a, 't>>,
    binders: &'a Binders<'t>,
    name: &str,
) -> Option<&'a BinderInfo> {
    binders.get(name).or_else(|| macros.iter().find_map(|state| state.binders.get(name)))
}

/// rustc's `ops_is_prefix`: a use need only be under its binder's repetitions, outermost
/// first, and maybe more.
fn ops_is_prefix(span: TextRange, name: Token, binder: &[Kleene], occurrence: &[Kleene]) -> Option<Misuse> {
    for (i, binder) in binder.iter().enumerate() {
        let Some(occurrence) = occurrence.get(i) else {
            return Some(Misuse::StillRepeating { span, binder: binder.range, name });
        };
        if occurrence.op != binder.op {
            return Some(Misuse::WrongOperator { span, binder: binder.range, occurrence: occurrence.range });
        }
    }
    None
}

/// How much of a macro definition `check_nested_occurrences` has read: rustc's
/// `NestedMacroState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nested {
    Empty,
    MacroRules,
    MacroRulesBang,
    MacroRulesBangName,
    Macro,
    MacroName,
    MacroNameParen,
}

#[cfg(test)]
mod tests {
    use span::Edition;
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::*;

    /// Each misuse in the definition `text`, as the text it is at.
    fn misuses(text: &str) -> Vec<String> {
        let file = SourceFile::parse(text, Edition::Edition2021);
        let def = file.root().descendants().find_map(ast::Macro::cast).unwrap();
        let tokens = Tokens::of(def, &|_| (Edition::Edition2021, true)).unwrap();
        let (m, rules) = Matchers::rules(&tokens, true, Edition::Edition2021);
        let mut checker = Checker::new(&m, &tokens);
        for rule in rules {
            if let (Some(lhs), Some(rhs)) = (rule.lhs, rule.rhs) {
                checker.rule(rule.args, lhs, rhs);
            }
        }
        let at = |range| tokens.text(range).to_owned();
        checker
            .misuses
            .iter()
            .map(|it| match *it {
                Misuse::Duplicate { span, prev } => format!("duplicate {} {}", at(span), at(prev)),
                Misuse::Unknown { span, .. } => format!("unknown {}", at(span)),
                Misuse::StillRepeating { span, binder, .. } => format!("repeating {} {}", at(span), at(binder)),
                Misuse::WrongOperator { span, binder, occurrence } => {
                    format!("operator {} {} {}", at(span), at(binder), at(occurrence))
                }
            })
            .collect()
    }

    #[test]
    fn a_use_must_be_bound_and_repeat_as_its_binder() {
        assert_eq!(misuses("macro_rules! m { ($($i:ident)*) => { $k $i $($i)+ $($($i)*)* }; }"), [
            "unknown $k",
            "repeating $i *",
            "operator $i * +",
        ]);
    }

    #[test]
    fn binders_reach_into_a_macro_a_transcriber_defines() {
        assert_eq!(
            misuses("macro_rules! m { ($($i:ident)*) => { $(macro_rules! $i { ($x:tt $x:tt) => { $x $y $i }; })* }; }"),
            ["duplicate $x $x", "unknown $y"],
        );
        assert_eq!(misuses("macro_rules! m { ($x:tt) => { macro_rules! n { ($x:tt) => { $x }; } }; }"), Vec::<String>::new());
    }

    #[test]
    fn a_variable_bound_twice_in_a_rule_is_an_error_not_a_lint() {
        assert!(misuses("macro_rules! m { ($x:tt $x:tt) => { $x }; }").is_empty());
    }

    #[test]
    fn a_metavar_expression_uses_its_variables() {
        assert_eq!(
            misuses("macro_rules! m { ($($x:tt)*) => { $(${ignore($x)})* ${ignore($y)} }; }"),
            ["unknown {ignore($y)}"],
        );
    }
}
