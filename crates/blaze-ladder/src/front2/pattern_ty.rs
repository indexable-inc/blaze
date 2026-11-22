//! Pattern types: core's built-in `pattern_type!(T is PAT)` (core's `Nanoseconds`,
//! `NonZero*Inner`, `NonNull`), which blaze-expand writes as `builtin # pattern_type(T is PAT)`
//! and the lowering keeps as ONE form, [`lf::TypeRef::Pat`], in every type position.
//!
//! A body is lowered expanding, so its pattern types arrive as that form. A signature is lowered
//! unexpanded: a type macro in it ([`lf::TypeRef::Macro`]) is expanded here, its own text
//! lowered through `blaze_lower::lower_type_text_expanding` with the names of the module it is
//! written in, and the result read as any type (a pattern type's base and pattern included).
//! An expansion that leaves a macro call is refused, so a head's type macro is expanded once
//! (bounded; no recursion past one level).
//!
//! The pattern is read by the checker lane's `parse_pattern_type` and `pattern_type` from its
//! written text: integer and `char` ranges with literal or path ends, ors of them, and `!null`.
//! A constant path end (`HALF_USIZE`, `u32::MAX`) is [`Why::ConstPath`] naming it until the
//! constant evaluator has a world over front2's records.

use std::alloc::Global;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_lower::form as lf;
use blaze_lower::typeck::{Refused, parse_pattern_type, pattern_type};
use blaze_lower::View;
use blaze_surface::Range;
use blaze_typeck::{PTy, PortableTys};

use super::source::NamesSource;
use super::types::{Builder, Env, Heads, Why};

fn refused(what: String, r: Range) -> Why {
    Why::PatternType(what.into_boxed_str(), r)
}

impl<'n, H: Heads, S: NamesSource<'n>> Builder<'_, H, S> {
    /// The type the unexpanded type macro `t` of a signature's `v` stands for, written in
    /// `env.module`: its text expanded and lowered, then read as a type.
    pub(super) fn macro_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, r: Range) -> Result<PTy, Why> {
        let text = v.text_at(v.type_range(t)?)?;
        let x = super::type_expand::expand_type_text(self.resolver.names, env.module, text, self.marks, r, tys.alloc())?;
        // A fn's return type with a macro in it is expanded whole by `heads.rs` `fn_head`
        // before its opaques are numbered, so no return opaque arrives here. An `impl Trait`
        // a type macro writes anywhere else (a parameter's type, a field, an alias) is refused
        // by name; it is never looked up in `env.anon`, whose ids are the item view's, not
        // this expansion's (a type id carries no view: a lookup could match another type).
        let mut impls: Vec<lf::TypeId, Global> = Vec::new_in(Global);
        x.impls(text, r, &mut impls)?;
        if !impls.is_empty() {
            return Err(Why::Unsupported("an `impl Trait` a type macro writes outside a fn's return type", r));
        }
        // The expansion is a text of its own, read at `env.module`: the item's marks are not
        // its tokens' (front2 item_site.rs).
        let own = Builder { resolver: crate::resolve::Paths { names: self.resolver.names }, heads: self.heads, marks: None };
        own.ty(tys, &Env { anon: &[], ..*env }, &x.view(), x.root)
    }

    /// The pattern type over `base` (already built) with the pattern `pat` of `v`.
    pub(super) fn pat_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, base: PTy, pat: lf::TyPat, r: Range) -> Result<PTy, Why> {
        let written = match pat {
            lf::TyPat::NotNull => String::from("_ is !null"),
            lf::TyPat::Of(p) => format!("_ is {}", v.text_at(v.pat_range(p)?)?),
        };
        let text = parse_pattern_type(&written, r).map_err(|e| refused(format!("{e:?}"), r))?;
        let names = self.resolver.names;
        let krate = names.crate_of(env.module).ok_or_else(|| refused(format!("the crate of module {:x} is not read", env.module), r))?;
        let atoms = names.cfg_atoms(krate).ok_or_else(|| refused(format!("the cfg of the crate of module {:x} is not read", env.module), r))?;
        // The target's pointer width, from the crate's cfg (`target_pointer_width`).
        let bits = atoms
            .iter()
            .find(|(n, _)| *n == "target_pointer_width")
            .and_then(|(_, w)| w.and_then(|w| w.parse::<u32>().ok()))
            .ok_or_else(|| refused(format!("the crate's cfg has no `target_pointer_width`"), r))?;
        // A path end (`HALF_USIZE` in core's `UsizeNoHighBit(usize is 0..=HALF_USIZE)`) is a
        // constant item's value, read as an array length's is: `Why::NeedConst` names it until
        // the evaluator has filled it (the caller's `eval_len` loop), then its value.
        // An end that is another constant expression (`u32::MAX`, `A - 1`) is an anonymous
        // constant of the BASE type (`Builder::end_value`), needed and evaluated the same way.
        let base_node = tys.view().get(base).copied();
        let mut need: Option<Why> = None;
        let mut eval = |p: &str| -> Result<u128, Refused> {
            match self.end_value(env, p, r, base_node) {
                Ok(value) => Ok(value),
                Err(why) => {
                    need = Some(why);
                    Err(Refused::Form { what: "a constant path end", range: r })
                }
            }
        };
        let made = pattern_type(tys, base, &text, bits, r, &mut eval);
        match (made, need) {
            (Ok(t), _) => Ok(t),
            (Err(_), Some(why)) => Err(why),
            (Err(e), None) => Err(refused(format!("{e:?}"), r)),
        }
    }
}
