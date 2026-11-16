//! The builtin derives, written here token to token, as rustc's `deriving` writes them: the
//! item is read by [`crate::adt`], and each impl is written straight into the expansion. What
//! the impl names of the item (its name, a variant's, a field's, the generic parameters, the
//! predicates, the field types' `T::Assoc` paths) is the item's own token, of origin
//! [`Origin::Input`], so it keeps the item's hygiene and edition; what the derive writes is
//! made def-site ([`Transparency::Opaque`]), as rustc's derives write at `def_site`.
//!
//! Not done yet: the `*self` body of `Clone` for a type that also derives `Copy` (the call does
//! not say what else the item derives; the impl written instead is equivalent), and
//! `derive_const`.

use core::{alloc::Allocator, fmt, ops::Range};

use crate::{
    adt::{self, Adt, AdtKind, FieldsKind, ParamKind, Variant},
    def::BuiltinDerive,
    error::ExpandError,
    expand::Out,
    expansion::{Diagnostic, Origin},
    literal::{self, Buf},
    tokens::{Delim, LitKind, Raw, Spacing, TokenKind, TokenStream, Transparency, index},
};

/// What the derives write: def-site.
const MADE: Transparency = Transparency::Opaque;

const UNIONS: &str = "this trait cannot be derived for unions";

/// Expands the derive `which` of the item in `out`.
pub(crate) fn expand<A: Allocator + Clone>(
    which: BuiltinDerive,
    out: &mut Out<'_, A>,
) -> Result<(), ExpandError> {
    let input = out.call.input();
    let adt = match adt::read(input, out.alloc.clone()) {
        Ok(adt) => adt,
        Err(error) => return refuse(out, error.message()),
    };
    let union = adt.kind == AdtKind::Union;
    let unions_refused = matches!(
        which,
        BuiltinDerive::Debug
            | BuiltinDerive::Default
            | BuiltinDerive::Hash
            | BuiltinDerive::Ord
            | BuiltinDerive::PartialOrd
            | BuiltinDerive::PartialEq
    );
    if union && unions_refused {
        return refuse(out, UNIONS);
    }
    let mut w = W { out, input, adt: &adt };
    match which {
        BuiltinDerive::Copy => w.marker("$crate::marker::Copy", false),
        BuiltinDerive::Eq => w.marker("$crate::cmp::Eq", adt.packed),
        BuiltinDerive::Clone => w.derive_clone(),
        BuiltinDerive::Default => w.derive_default(),
        BuiltinDerive::Debug => w.derive_debug(),
        BuiltinDerive::Hash => w.derive_hash(),
        BuiltinDerive::PartialEq => w.derive_partial_eq(),
        BuiltinDerive::Ord => w.derive_ord(false),
        BuiltinDerive::PartialOrd => w.derive_ord(true),
    }
}

/// The derive's error, `message`; it makes nothing else.
fn refuse<A: Allocator + Clone>(out: &mut Out<'_, A>, message: &str) -> Result<(), ExpandError> {
    let message = out.note(message)?;
    out.diagnose(Diagnostic::Derive { message });
    Ok(())
}

/// Which of the two values a comparison binds a field of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    This,
    Other,
}

impl Side {
    /// What a binding of a field is named after it: a field's name may be that of a unit struct
    /// or a constant in scope, which a binding cannot have.
    fn suffix(self) -> &'static str {
        match self {
            Side::This => "_self",
            Side::Other => "_other",
        }
    }

    fn place(self) -> &'static str {
        match self {
            Side::This => "self",
            Side::Other => "other",
        }
    }
}

/// The writer of one derive's impls into the expansion.
struct W<'w, 'c, A: Allocator + Clone> {
    out: &'w mut Out<'c, A>,
    input: &'c TokenStream<A>,
    adt: &'w Adt<A>,
}

type Written = Result<(), ExpandError>;

impl<A: Allocator + Clone> W<'_, '_, A> {
    /// Writes `code`, made: a run of letters, digits, `_` and `$` is an identifier (`$crate`
    /// one), each other character a punct, joined to the next when that is a punct too, and a
    /// bracket a group's delimiter. Spaces only separate. `code` is ASCII.
    fn code(&mut self, code: &str) -> Written {
        let bytes = code.as_bytes();
        let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
        let delim = |byte: u8| b"()[]{}".contains(&byte);
        let mut at = 0;
        while let Some(&byte) = bytes.get(at) {
            if byte == b' ' {
                at += 1;
                continue;
            }
            if word(byte) {
                let start = at;
                while bytes.get(at).is_some_and(|&it| word(it)) {
                    at += 1;
                }
                let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
                self.out.made(ident, &code[start..at], MADE)?;
                continue;
            }
            let kind = match byte {
                b'(' => TokenKind::Open(Delim::Paren),
                b')' => TokenKind::Close(Delim::Paren),
                b'[' => TokenKind::Open(Delim::Bracket),
                b']' => TokenKind::Close(Delim::Bracket),
                b'{' => TokenKind::Open(Delim::Brace),
                b'}' => TokenKind::Close(Delim::Brace),
                _ => {
                    let joined = bytes.get(at + 1).is_some_and(|&next| {
                        next.is_ascii_punctuation() && !word(next) && !delim(next)
                    });
                    let spacing = if joined { Spacing::Joint } else { Spacing::Alone };
                    TokenKind::Punct { char: char::from(byte), spacing }
                }
            };
            self.out.made(kind, "", MADE)?;
            at += 1;
        }
        Ok(())
    }

    /// The item's token at `at`.
    fn input(&mut self, at: usize) -> Written {
        let token = &self.input.tokens()[at];
        self.out.push(token.kind, self.input.text_of(token), Origin::Input(index(at)))
    }

    /// The item's tokens `range`, the last one not joined to what is written after it.
    fn range(&mut self, range: &Range<usize>) -> Written {
        for at in range.clone() {
            let token = &self.input.tokens()[at];
            let kind = if at + 1 == range.end {
                token.kind.with_spacing(Spacing::Alone)
            } else {
                token.kind
            };
            self.out.push(kind, self.input.text_of(token), Origin::Input(index(at)))?;
        }
        Ok(())
    }

    /// A made identifier whose name `name` writes.
    fn made_ident(&mut self, name: impl FnOnce(&mut Buf<A>)) -> Written {
        let mut text = Buf::new_in(self.out.alloc.clone());
        name(&mut text);
        let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
        self.out.made(ident, text.as_str(), MADE)
    }

    /// The binding of field `field` of `side`: the field's name, or `f{i}` for tuple field `i`,
    /// then the side's suffix.
    fn binding(&mut self, field: usize, side: Side) -> Written {
        let (input, adt) = (self.input, self.adt);
        let field = &adt.fields[field];
        self.made_ident(|text| {
            match field.name {
                Some(at) => text.push_str(input.text_at(at)),
                None => fmt::Write::write_fmt(text, format_args!("f{}", field.index))
                    .expect("a `Buf` takes any text"),
            }
            text.push_str(side.suffix());
        })
    }

    /// A made string literal of the name the item's token `at` is (`r#` not included).
    fn name_literal(&mut self, at: usize) -> Written {
        let mut text = Buf::new_in(self.out.alloc.clone());
        literal::escape_str(self.input.text_at(at), &mut text);
        let kind =
            TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
        self.out.made(kind, text.as_str(), MADE)
    }

    /// A made integer literal, a tuple field's index.
    fn index_literal(&mut self, value: usize) -> Written {
        let mut text = Buf::new_in(self.out.alloc.clone());
        fmt::Write::write_fmt(&mut text, format_args!("{value}")).expect("a `Buf` takes any text");
        let kind =
            TokenKind::Literal { kind: LitKind::Integer, suffix_len: 0, spacing: Spacing::Alone };
        self.out.made(kind, text.as_str(), MADE)
    }

    /// `#[automatically_derived] impl<..> Trait for Name<..> where ..`, the trait's path
    /// `path`. With `bound`, each type parameter and each `T::Assoc` of the fields is bounded
    /// by the trait; with `copy`, each type parameter and `T::Assoc` by `Copy` too, after the
    /// item's own predicates, as rustc's derives of a packed struct do
    /// (`needs_copy_as_bound_if_packed`): its fields are copied out.
    fn header(&mut self, path: &str, bound: bool, copy: bool) -> Written {
        let adt = self.adt;
        self.out.automatically_derived()?;
        self.code("impl")?;
        if !adt.params.is_empty() {
            self.code("<")?;
            for param in &adt.params {
                match &param.kind {
                    ParamKind::Lifetime => self.range(&param.decl)?,
                    ParamKind::Const { ty } => {
                        self.code("const")?;
                        self.range(&param.name)?;
                        self.code(":")?;
                        self.range(ty)?;
                    }
                    ParamKind::Type { .. } => {
                        self.range(&param.name)?;
                        if bound {
                            self.code(":")?;
                            self.code(path)?;
                        }
                    }
                }
                self.code(",")?;
            }
            self.code(">")?;
        }
        self.code(path)?;
        self.code("for")?;
        self.input(adt.name)?;
        if !adt.params.is_empty() {
            self.code("<")?;
            for param in &adt.params {
                self.range(&param.name)?;
                self.code(",")?;
            }
            self.code(">")?;
        }
        let types = || adt.params.iter().filter(|it| matches!(it.kind, ParamKind::Type { .. }));
        let bounded = types().any(|it| matches!(it.kind, ParamKind::Type { bounds: Some(_) }));
        let copies = copy && (types().next().is_some() || !adt.assoc.is_empty());
        let traits = bound && !adt.assoc.is_empty();
        if adt.predicates.is_empty() && !bounded && !copies && !traits {
            return Ok(());
        }
        self.code("where")?;
        for predicate in &adt.predicates {
            self.range(predicate)?;
            self.code(",")?;
        }
        if copy {
            for param in types() {
                self.range(&param.name)?;
                self.code(": $crate::marker::Copy,")?;
            }
            for ty in &adt.assoc {
                self.range(ty)?;
                self.code(": $crate::marker::Copy,")?;
            }
        }
        for param in types() {
            if let ParamKind::Type { bounds: Some(bounds) } = &param.kind {
                self.range(&param.name)?;
                self.code(":")?;
                self.range(bounds)?;
                self.code(",")?;
            }
        }
        if bound {
            for ty in &adt.assoc {
                self.range(ty)?;
                self.code(":")?;
                self.code(path)?;
                self.code(",")?;
            }
        }
        Ok(())
    }

    /// A marker trait's impl: `Copy`, `Eq`.
    fn marker(&mut self, path: &str, copy: bool) -> Written {
        self.header(path, true, copy)?;
        self.code("{ }")
    }

    /// `Name` or `Name::Variant`.
    fn path(&mut self, v: &Variant) -> Written {
        let adt = self.adt;
        self.input(adt.name)?;
        if adt.is_enum() {
            self.code("::")?;
            self.input(v.name)?;
        }
        Ok(())
    }

    /// The variant's pattern or expression: its path, and what `field` writes in the place of
    /// each field (by its index in [`Adt::fields`]).
    fn variant(&mut self, v: &Variant, field: impl Fn(&mut Self, usize) -> Written) -> Written {
        let adt = self.adt;
        self.path(v)?;
        match v.kind {
            FieldsKind::Unit => Ok(()),
            FieldsKind::Record => {
                self.code("{")?;
                for i in v.fields.clone() {
                    if let Some(name) = adt.fields[i].name {
                        self.input(name)?;
                        self.code(":")?;
                    }
                    field(self, i)?;
                    self.code(",")?;
                }
                self.code("}")
            }
            FieldsKind::Tuple => {
                self.code("(")?;
                for i in v.fields.clone() {
                    field(self, i)?;
                    self.code(",")?;
                }
                self.code(")")
            }
        }
    }

    /// The variant's pattern, which binds each field of `side`.
    fn pattern(&mut self, v: &Variant, side: Side) -> Written {
        self.variant(v, |w, i| w.binding(i, side))
    }

    /// For a packed struct, a `let` of each field of `side` to a copy out of it, bound to the
    /// name the patterns of the other derives bind it to. The field's name is the item's token.
    fn packed_lets(&mut self, side: Side) -> Written {
        let adt = self.adt;
        let Some(v) = adt.variants.first() else { return Ok(()) };
        for i in v.fields.clone() {
            let field = &adt.fields[i];
            self.code("let")?;
            self.binding(i, side)?;
            self.code("= {")?;
            self.code(side.place())?;
            self.code(".")?;
            match field.name {
                Some(name) => self.input(name)?,
                None => self.index_literal(field.index)?,
            }
            self.code("};")?;
        }
        Ok(())
    }

    /// A `match` of `self` against each variant's pattern, with the arm `body` writes for it.
    /// A packed struct's fields are copied out of it instead, as rustc's derives do
    /// (`{ self.x }`): a field may be misaligned, so is not borrowed.
    fn match_self(&mut self, body: impl Fn(&mut Self, &Variant) -> Written) -> Written {
        let adt = self.adt;
        if adt.packed_struct() {
            self.code("{")?;
            self.packed_lets(Side::This)?;
            body(self, &adt.variants[0])?;
            return self.code("}");
        }
        self.code("match self {")?;
        for v in &adt.variants {
            self.pattern(v, Side::This)?;
            self.code("=>")?;
            body(self, v)?;
            self.code(",")?;
        }
        self.code("}")
    }

    /// A `match` of `self` and `other` against each of `arms`' two patterns, with the arm
    /// `body` writes for it, and `fallback` if none matches. Of one variant whose arm is
    /// written (a struct, an enum of one variant) the match has no fallback, which nothing
    /// could reach: rustc's derives write none there.
    fn match_both<'v>(
        &mut self,
        arms: impl Iterator<Item = &'v Variant>,
        body: impl Fn(&mut Self, &Variant) -> Written,
        fallback: impl FnOnce(&mut Self) -> Written,
    ) -> Written {
        let adt = self.adt;
        if adt.packed_struct() {
            self.code("{")?;
            self.packed_lets(Side::This)?;
            self.packed_lets(Side::Other)?;
            body(self, &adt.variants[0])?;
            return self.code("}");
        }
        self.code("match (self, other) {")?;
        let mut written = 0usize;
        for v in arms {
            written += 1;
            self.code("(")?;
            self.pattern(v, Side::This)?;
            self.code(",")?;
            self.pattern(v, Side::Other)?;
            self.code(") =>")?;
            body(self, v)?;
            self.code(",")?;
        }
        if !(adt.variants.len() == 1 && written == 1) {
            self.code("_unused =>")?;
            fallback(self)?;
        }
        self.code("}")
    }

    fn derive_clone(&mut self) -> Written {
        let adt = self.adt;
        self.header("$crate::clone::Clone", true, adt.packed)?;
        self.code("{ fn clone(&self) -> Self {")?;
        if adt.kind == AdtKind::Union {
            self.code("*self")?;
        } else if adt.is_empty_enum() {
            self.code("match *self {}")?;
        } else {
            self.match_self(|w, v| {
                w.variant(v, |w, i| {
                    // A packed struct's field is a copy out of it, cloned by reference.
                    w.code(if adt.packed {
                        "$crate::clone::Clone::clone(&"
                    } else {
                        "$crate::clone::Clone::clone("
                    })?;
                    w.binding(i, Side::This)?;
                    w.code(")")
                })
            })?;
        }
        self.code("} }")
    }

    fn derive_default(&mut self) -> Written {
        let adt = self.adt;
        let (v, bound) = match adt.kind {
            AdtKind::Struct => (&adt.variants[0], true),
            AdtKind::Enum { default_variant: Some(at) } => (&adt.variants[at], false),
            AdtKind::Enum { default_variant: None } => {
                return refuse(self.out, "no default declared");
            }
            AdtKind::Union => return refuse(self.out, UNIONS),
        };
        self.header("$crate::default::Default", bound, false)?;
        self.code("{ fn default() -> Self {")?;
        self.variant(v, |w, _| w.code("$crate::default::Default::default()"))?;
        self.code("} }")
    }

    fn derive_debug(&mut self) -> Written {
        let adt = self.adt;
        self.header("$crate::fmt::Debug", true, adt.packed)?;
        self.code("{ fn fmt(&self, f: &mut $crate::fmt::Formatter) -> $crate::fmt::Result {")?;
        if adt.is_empty_enum() {
            self.code("match *self {}")?;
            return self.code("} }");
        }
        // Spelled out in full, as rustc does: a method of the same name on another trait must
        // not be picked up.
        self.match_self(|w, v| {
            let (start, field, finish) = match v.kind {
                FieldsKind::Unit => {
                    w.code("$crate::fmt::Formatter::write_str(f,")?;
                    w.name_literal(v.name)?;
                    return w.code(")");
                }
                FieldsKind::Record => (
                    "{ let mut ra_expand_builder = $crate::fmt::Formatter::debug_struct(f,",
                    "$crate::fmt::DebugStruct::field(&mut ra_expand_builder,",
                    "$crate::fmt::DebugStruct::finish(&mut ra_expand_builder) }",
                ),
                FieldsKind::Tuple => (
                    "{ let mut ra_expand_builder = $crate::fmt::Formatter::debug_tuple(f,",
                    "$crate::fmt::DebugTuple::field(&mut ra_expand_builder,",
                    "$crate::fmt::DebugTuple::finish(&mut ra_expand_builder) }",
                ),
            };
            w.code(start)?;
            w.name_literal(v.name)?;
            w.code(");")?;
            for i in v.fields.clone() {
                w.code(field)?;
                if let Some(name) = adt.fields[i].name {
                    w.name_literal(name)?;
                    w.code(",")?;
                }
                // The `&` rustc's `Debug` puts before the last field of a packed struct, which
                // may be unsized: a `&dyn Debug` of a reference to the copy out of it.
                w.code(if adt.packed && i + 1 == v.fields.end { "& &" } else { "&" })?;
                w.binding(i, Side::This)?;
                w.code(");")?;
            }
            w.code(finish)
        })?;
        self.code("} }")
    }

    fn derive_hash(&mut self) -> Written {
        let adt = self.adt;
        // The hasher is `__H`, as rustc's, so a type with a parameter `H` derives it too.
        self.header("$crate::hash::Hash", true, adt.packed)?;
        self.code("{ fn hash<__H: $crate::hash::Hasher>(&self, ra_expand_state: &mut __H) {")?;
        if adt.is_empty_enum() {
            self.code("match *self {}")?;
            return self.code("} }");
        }
        // rustc leaves the discriminant out when there is only one variant.
        if adt.is_enum() && adt.variants.len() > 1 {
            self.code(
                "$crate::hash::Hash::hash(&$crate::mem::discriminant(self), ra_expand_state);",
            )?;
        }
        self.match_self(|w, v| {
            w.code("{")?;
            for i in v.fields.clone() {
                // The binding is the field's reference, as rustc's `__self_0` is; a packed
                // struct's field is a copy out of it, hashed by reference.
                w.code(if adt.packed {
                    "$crate::hash::Hash::hash(&"
                } else {
                    "$crate::hash::Hash::hash("
                })?;
                w.binding(i, Side::This)?;
                w.code(", ra_expand_state);")?;
            }
            w.code("}")
        })?;
        self.code("} }")
    }

    /// `StructuralPartialEq`, which bounds no parameter, and `PartialEq`, as rustc's
    /// `expand_deriving_partial_eq`.
    fn derive_partial_eq(&mut self) -> Written {
        let adt = self.adt;
        self.header("$crate::marker::StructuralPartialEq", false, false)?;
        self.code("{ }")?;
        self.header("$crate::cmp::PartialEq", true, adt.packed)?;
        self.code("{ fn eq(&self, other: &Self) -> bool {")?;
        // An enum of several variants compares the discriminants, then the fields of the
        // variants that have any.
        let discriminants = adt.is_enum() && adt.variants.len() > 1;
        let arms = || adt.variants.iter().filter(move |v| !discriminants || !v.is_empty());
        if discriminants {
            self.code(
                "let __self_discr = $crate::intrinsics::discriminant_value(self); \
                 let __arg1_discr = $crate::intrinsics::discriminant_value(other); \
                 __self_discr == __arg1_discr",
            )?;
            if arms().next().is_some() {
                self.code("&&")?;
                self.match_both(arms(), Self::fields_eq, |w| w.code("true"))?;
            }
        } else {
            self.match_both(arms(), Self::fields_eq, |w| w.code("false"))?;
        }
        self.code("} }")
    }

    /// Whether the fields of `self`'s and `other`'s variant `v` are equal, by path, as rustc
    /// does: a type's own `eq` would be found first.
    fn fields_eq(&mut self, v: &Variant) -> Written {
        if v.is_empty() {
            return self.code("true");
        }
        for i in v.fields.clone() {
            if i != v.fields.start {
                self.code("&&")?;
            }
            self.code("$crate::cmp::PartialEq::eq(&")?;
            self.binding(i, Side::This)?;
            self.code(", &")?;
            self.binding(i, Side::Other)?;
            self.code(")")?;
        }
        Ok(())
    }

    /// `Ord` (`PartialOrd` for `partial`): for an enum of several variants the discriminants
    /// first, then the fields of the variants that have any, each in turn. As rustc's: a struct
    /// or an enum of one variant compares its fields alone.
    fn derive_ord(&mut self, partial: bool) -> Written {
        let adt = self.adt;
        if partial {
            self.header("$crate::cmp::PartialOrd", true, adt.packed)?;
            self.code(
                "{ fn partial_cmp(&self, other: &Self) -> \
                 $crate::option::Option<$crate::cmp::Ordering> {",
            )?;
        } else {
            self.header("$crate::cmp::Ord", true, adt.packed)?;
            self.code("{ fn cmp(&self, other: &Self) -> $crate::cmp::Ordering {")?;
        }
        let discriminants = adt.is_enum() && adt.variants.len() > 1;
        if discriminants {
            self.compare_open(partial)?;
            self.code("&$crate::intrinsics::discriminant_value(self), &$crate::intrinsics::discriminant_value(other)")?;
            self.compare_equal(partial)?;
        }
        let arms = adt.variants.iter().filter(|v| !discriminants || !v.is_empty());
        let arm = |w: &mut Self, v: &Variant| -> Written {
            for i in v.fields.clone() {
                w.compare_open(partial)?;
                w.code("&")?;
                w.binding(i, Side::This)?;
                w.code(", &")?;
                w.binding(i, Side::Other)?;
                w.compare_equal(partial)?;
            }
            w.equal(partial)?;
            for _ in v.fields.clone() {
                w.compare_close()?;
            }
            Ok(())
        };
        self.match_both(arms, arm, |w| w.equal(partial))?;
        if discriminants {
            self.compare_close()?;
        }
        self.code("} }")
    }

    /// `match Ord::cmp(`, before the two operands.
    fn compare_open(&mut self, partial: bool) -> Written {
        self.code(if partial {
            "match $crate::cmp::PartialOrd::partial_cmp("
        } else {
            "match $crate::cmp::Ord::cmp("
        })
    }

    /// `) { Equal => {`, after the two operands, up to where what follows when they are equal
    /// goes; [`Self::compare_close`] ends it.
    fn compare_equal(&mut self, partial: bool) -> Written {
        self.code(") {")?;
        self.equal(partial)?;
        self.code("=> {")
    }

    /// `} c => return c, }`.
    fn compare_close(&mut self) -> Written {
        self.code("} c => return c, }")
    }

    /// `Ordering::Equal`, or `Option::Some(Ordering::Equal)` for `partial`.
    fn equal(&mut self, partial: bool) -> Written {
        self.code(if partial {
            "$crate::option::Option::Some($crate::cmp::Ordering::Equal)"
        } else {
            "$crate::cmp::Ordering::Equal"
        })
    }
}
