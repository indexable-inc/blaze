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

use blaze_lower::by_name::ByName;
use blaze_lower::form as lf;
use blaze_lower::scoped::Scoped;
use blaze_lower::typeck::{Refused, parse_pattern_type, pattern_type};
use blaze_lower::View;
use blaze_own::Alloc;
use blaze_surface::Range;
use blaze_typeck::{PTy, PortableTys};

use super::body::NoHost;
use super::heads::CrateCfg;
use super::types::{Builder, Env, Heads, Why, children};

fn refused(what: String, r: Range) -> Why {
    Why::PatternType(what.into_boxed_str(), r)
}

impl<H: Heads> Builder<'_, '_, H> {
    /// The type the unexpanded type macro `t` of a signature's `v` stands for, written in
    /// `env.module`: its text expanded and lowered, then read as a type.
    pub(super) fn macro_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, r: Range) -> Result<PTy, Why> {
        let text = v.text_at(v.type_range(t)?)?;
        let names = self.resolver.names;
        let krate = names.crate_of(env.module).ok_or_else(|| refused(format!("the crate of module {:x} is not read", env.module), r))?;
        let atoms = names.krate(krate).cfg.atoms();
        let scoped = Scoped { names, module: env.module, prepared: None };
        let mut expand = ByName::with_names(&scoped, NoHost, Alloc::global());
        let lowered = blaze_lower::lower_type_text_expanding(text, names.edition(krate), &mut CrateCfg(atoms), &mut expand, Alloc::global())
            .map_err(|e| Why::TypeMacro(format!("`{text}`: {e}").into_boxed_str(), r))?;
        let bv = lowered.view();
        let root = bv.type_root().ok_or_else(|| Why::TypeMacro(format!("`{text}` expanded to no type").into_boxed_str(), r))?;
        let mut stack: Vec<lf::TypeId, Global> = Vec::new_in(Global);
        let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
        stack.push(root);
        while let Some(t) = stack.pop() {
            match bv.ty(t)? {
                lf::TypeRef::Macro(_) => return Err(Why::TypeMacro(format!("`{text}` (a macro call its expansion leaves)").into_boxed_str(), r)),
                lf::TypeRef::Error => return Err(Why::TypeMacro(format!("`{text}` (its expansion is no type)").into_boxed_str(), r)),
                _ => {}
            }
            kids.clear();
            children(&bv, t, &mut kids)?;
            stack.extend(kids.iter().copied());
        }
        self.ty(tys, env, &bv, root)
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
        let atoms = names.krate(krate).cfg.atoms();
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
        let base_node = tys.view().nodes.get(base.0 as usize).copied();
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
