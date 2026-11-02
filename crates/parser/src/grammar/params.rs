use crate::grammar::attributes::OUTER_ATTR_FIRST;

use super::*;

// test param_list
// fn a() {}
// fn b(x: i32) {}
// fn c(x: i32, ) {}
// fn d(x: i32, y: ()) {}

// test_err empty_param_slot
// fn f(y: i32, ,t: i32) {}
pub(super) fn param_list_fn_def(p: &mut Parser<'_>, is_in_trait: bool) {
    // Before the 2018 edition a method of a trait may leave its parameters unnamed, as
    // rustc's `parse_trait_item` passes `|edition| edition >= Edition::Edition2018` for
    // `req_name`.
    let unnamed = is_in_trait && !p.current_edition().at_least_2018();
    list_(p, if unnamed { Flavor::TraitFn2015 } else { Flavor::FnDef });
}

pub(super) fn param_list_fn_ptr(p: &mut Parser<'_>) {
    list_(p, Flavor::FnPointer);
}

pub(super) fn param_list_closure(p: &mut Parser<'_>) {
    list_(p, Flavor::Closure);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    FnDef,
    /// A method of a trait before the 2018 edition, whose parameters may be types alone.
    TraitFn2015,
    FnPointer,
    Closure,
}

fn list_(p: &mut Parser<'_>, flavor: Flavor) {
    use Flavor::*;

    let (bra, ket) = match flavor {
        Closure => (T![|], T![|]),
        FnDef | TraitFn2015 | FnPointer => (T!['('], T![')']),
    };

    let list_marker = p.start();
    p.bump(bra);

    let mut param_marker = None;
    if let FnDef | TraitFn2015 | FnPointer = flavor {
        // test self_param_outer_attr
        // fn f(#[must_use] self) {}

        // test fn_pointer_self_param
        // type A = fn(&self);
        // type B = fn(mut self: u8, u8);
        let m = p.start();
        attributes::outer_attrs(p);
        match opt_self_param(p, m) {
            Ok(()) => {}
            Err(m) => param_marker = Some(m),
        }
    }

    while !p.at(EOF) && !p.at(ket) {
        // test param_outer_arg
        // fn f(#[attr1] pat: Type) {}
        let m = match param_marker.take() {
            Some(m) => m,
            None => {
                let m = p.start();
                attributes::outer_attrs(p);
                m
            }
        };

        if !p.at_ts(PARAM_FIRST.union(OUTER_ATTR_FIRST)) {
            p.error("expected value parameter");
            m.abandon(p);
            if p.eat(T![,]) {
                continue;
            }
            break;
        }
        param(p, m, flavor);
        if !p.eat(T![,]) {
            if p.at_ts(PARAM_FIRST.union(OUTER_ATTR_FIRST)) {
                p.error("expected `,`");
            } else {
                break;
            }
        }
    }

    if let Some(m) = param_marker {
        m.abandon(p);
    }

    p.expect(ket);
    list_marker.complete(p, PARAM_LIST);
}

const PARAM_FIRST: TokenSet = patterns::PATTERN_FIRST.union(types::TYPE_FIRST);

fn param(p: &mut Parser<'_>, m: Marker, flavor: Flavor) {
    match flavor {
        // test param_list_vararg
        // extern "C" { fn printf(format: *const i8, ..., _: u8) -> i32; }
        Flavor::FnDef | Flavor::TraitFn2015 | Flavor::FnPointer if p.eat(T![...]) => {}

        // test trait_fn_unnamed_params 2015
        // trait T { fn f(&self, *const u8, Option<&u8>, x: u8, _: &u8, &u8) -> u8; }
        Flavor::TraitFn2015 if !is_named_param(p) && !is_ref_mut_named_param(p) => {
            types::type_(p)
        }

        // test fn_def_param
        // fn foo(..., (x, y): (i32, i32)) {}
        Flavor::FnDef | Flavor::TraitFn2015 => {
            patterns::pattern(p);
            if !variadic_param(p) {
                if p.at(T![:]) {
                    types::ascription(p);
                } else {
                    // test_err missing_fn_param_type
                    // fn f(x y: i32, z, t: i32) {}
                    p.error("missing type for function parameter");
                }
            }
        }
        // test fn_pointer_param_ident_path
        // type Foo = fn(Bar::Baz);
        // type Qux = fn(baz: Bar::Baz);

        // test fn_pointer_unnamed_arg
        // type Foo = fn(_: bar);

        // test fn_pointer_param_pat
        // type A = fn(mut a: u8, &b: u8, &&c: u8, r#ref: u8);

        // test_err fn_pointer_param_ref_mut_pat
        // type A = fn(&mut a: u8);
        Flavor::FnPointer => {
            let ref_mut = !is_named_param(p) && is_ref_mut_named_param(p);
            if ref_mut || is_named_param(p) {
                patterns::pattern_single(p);
                if ref_mut {
                    // rustc's `recover_arg_parse` (E0642), which reads the pattern only after
                    // `&mut a` failed as a type.
                    p.error("patterns aren't allowed in function pointer types");
                }
                if !variadic_param(p) {
                    if p.at(T![:]) {
                        types::ascription(p);
                    } else {
                        p.error("missing type for function parameter");
                    }
                }
            } else {
                types::type_(p);
            }
        }
        // test closure_params
        // fn main() {
        //    let foo = |bar, baz: Baz, qux: Qux::Quux| ();
        // }
        Flavor::Closure => {
            patterns::pattern_single(p);
            if p.at(T![:]) && !p.at(T![::]) {
                types::ascription(p);
            }
        }
    }
    m.complete(p, PARAM);
}

/// rustc's `Parser::is_named_param`, asked of a parameter that may be a type alone: whether it
/// starts `x:`, `&x:`, `&&x:` or `mut x:`, where `x` is any identifier, keywords and `_`
/// included.
fn is_named_param(p: &Parser<'_>) -> bool {
    let offset = if p.at(T![&&]) {
        2
    } else if p.at(T![&]) || p.at(T![mut]) {
        1
    } else {
        0
    };
    is_name_before_colon(p, offset)
}

/// Whether a parameter starts `&mut x:`, which rustc's `is_named_param` misses: it parses `&mut x`
/// as a type, fails at the `:` and reads it again as a pattern.
fn is_ref_mut_named_param(p: &Parser<'_>) -> bool {
    p.at(T![&]) && p.nth_at(1, T![mut]) && is_name_before_colon(p, 2)
}

fn is_name_before_colon(p: &Parser<'_>, n: usize) -> bool {
    let name = p.nth(n);
    (name.is_any_identifier() || name == T![_])
        && p.nth_at(n + 1, T![:])
        && !p.nth_at(n + 1, T![::])
}

fn variadic_param(p: &mut Parser<'_>) -> bool {
    if p.at(T![:]) && p.nth_at(1, T![...]) {
        p.bump(T![:]);
        p.bump(T![...]);
        true
    } else {
        false
    }
}

// test self_param
// impl S {
//     fn a(self) {}
//     fn b(&self,) {}
//     fn c(&'a self,) {}
//     fn d(&'a mut self, x: i32) {}
//     fn e(mut self) {}
// }
fn opt_self_param(p: &mut Parser<'_>, m: Marker) -> Result<(), Marker> {
    let is_isolated_self = |p: &mut Parser<'_>, n| {
        // test non_isolated_self
        // fn f(self::S: S) {}
        // fn g(&self::S: &S) {}
        // fn h(&mut self::S: &mut S) {}

        // test_err non_isolated_self_err
        // fn f(mut self::S: S) {}
        // fn g(&'l self::S: &S) {}
        // fn h(&'l mut self::S: &mut S) {}
        p.nth_at(n, T![self]) && !p.nth_at(n + 1, T![::])
    };
    let mut self_pos = 0;
    if p.at(T![mut]) {
        self_pos += 1;
    }
    if is_isolated_self(p, self_pos) {
        p.eat(T![mut]);
        self_as_name(p);
        // test arb_self_types
        // impl S {
        //     fn a(self: &Self) {}
        //     fn b(mut self: Box<Self>) {}
        // }
        if p.at(T![:]) {
            types::ascription(p);
        }
    } else {
        if !p.at(T![&]) {
            return Err(m);
        }
        let mut self_pos = 1;
        if p.nth_at(self_pos, LIFETIME_IDENT) {
            self_pos += 1;
        }
        if p.nth_at(self_pos, T![mut]) {
            self_pos += 1;
        }
        if !is_isolated_self(p, self_pos) {
            return Err(m);
        }

        p.bump(T![&]);
        if p.at(LIFETIME_IDENT) {
            lifetime(p);
        }
        p.eat(T![mut]);
        self_as_name(p);
    }
    m.complete(p, SELF_PARAM);
    if !p.at(T![')']) {
        p.expect(T![,]);
    }
    Ok(())
}

fn self_as_name(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(T![self]);
    m.complete(p, NAME);
}
