use super::*;

pub(super) const OUTER_ATTR_FIRST: TokenSet = TokenSet::new(&[T![#], OUTER_DOC_COMMENT]);

pub(super) fn inner_attrs<S: Sink>(p: &mut Parser<'_, S>) {
    while p.at(INNER_DOC_COMMENT) || (p.at(T![#]) && p.nth_at(1, T![!])) {
        attr(p, true);
    }
}

pub(super) fn outer_attrs<S: Sink>(p: &mut Parser<'_, S>) {
    while p.at_ts(OUTER_ATTR_FIRST) {
        attr(p, false);
    }
}

fn attr<S: Sink>(p: &mut Parser<'_, S>, inner: bool) {
    if (inner && p.at(INNER_DOC_COMMENT)) || (!inner && p.at(OUTER_DOC_COMMENT)) {
        let m = p.start();
        p.bump_any();
        m.complete(p, DOC_COMMENT);
        return;
    }

    let attr = p.start();

    p.bump(T![#]);

    if inner {
        p.bump(T![!]);
    }

    if p.expect(T!['[']) {
        meta(p);
        p.expect(T![']']);
    }

    attr.complete(p, ATTR);
}

// test_err meta_recovery
// #![]
// #![p = ]
// #![p::]
// #![p:: =]
// #![unsafe]
// #![unsafe =]

const CFG_PREDICATE_FIRST_SET: TokenSet =
    expressions::LITERAL_FIRST.union(TokenSet::new(&[T![ident]]));

fn next_cfg_predicate_arg<S: Sink>(p: &mut Parser<'_, S>) -> bool {
    while !p.at(T![')']) && !p.at(EOF) {
        if p.at(T![,]) {
            let m = p.start();
            p.error("expected a cfg predicate");
            p.bump(T![,]);
            m.complete(p, ERROR);
            continue;
        }
        return p.at_ts(CFG_PREDICATE_FIRST_SET);
    }
    false
}

pub(super) fn cfg_predicate<S: Sink>(p: &mut Parser<'_, S>) {
    blaze_mem::scratch::with_u32s(|stack| {
        let mut close_first = false;
        'parse: loop {
            let m = p.start();
            if p.eat(T![true]) || p.eat(T![false]) {
                // test cfg_true_false_pred
                // #![cfg(true)]
                // #![cfg(false)]
                m.complete(p, CFG_ATOM);
            } else if p.at_ts(expressions::LITERAL_FIRST) {
                // rustc's `parse_cfg_entry` takes any literal here and refuses all but `version`'s
                // argument after parsing, so the error is the attribute checks', not the parser's.

                // test cfg_literal_pred
                // #![cfg(version("1.42"))]
                // #![cfg(any(1, "a", b'c'))]
                expressions::literal(p);
                m.complete(p, CFG_ATOM);
            } else {
                p.expect(T![ident]);
                if p.eat(T![=]) {
                    if p.at(T![ident]) {
                        // This is required for completion, that inserts an identifier, to work in cases like
                        // `#[cfg(key = $0)]`, and also makes sense on itself.

                        // test_err key_ident_cfg_predicate
                        // #![cfg(key = value)]
                        p.err_and_bump("expected a string literal");
                    } else {
                        // test cfg_key_value_pred
                        // #![cfg(key = "value")]
                        p.expect(T![string]);
                    }
                    m.complete(p, CFG_ATOM);
                } else if p.at(T!['(']) {
                    // test cfg_composite_pred
                    // #![cfg(any(a, all(b = "c", d)))]
                    p.bump(T!['(']);
                    stack.push(m.into_raw());
                    if next_cfg_predicate_arg(p) {
                        continue 'parse;
                    }
                    close_first = true;
                } else {
                    m.complete(p, CFG_ATOM);
                }
            }

            while let Some(raw) = stack.pop() {
                if close_first {
                    close_first = false;
                } else {
                    let mut more = true;
                    if !p.eat(T![,]) {
                        if p.at_ts(CFG_PREDICATE_FIRST_SET) {
                            p.error("expected COMMA");
                        } else {
                            more = false;
                        }
                    }
                    if more && next_cfg_predicate_arg(p) {
                        stack.push(raw);
                        continue 'parse;
                    }
                }
                p.expect(T![')']);
                Marker::from_raw(raw).complete(p, CFG_COMPOSITE);
            }
            return;
        }
    });
}

fn cfg_meta<S: Sink>(p: &mut Parser<'_, S>, m: Marker) {
    // test cfg_meta
    // #![cfg(foo)]
    // #![cfg(foo = "bar",)]
    p.eat_contextual_kw(T![cfg]);
    p.bump(T!['(']);
    cfg_predicate(p);
    p.eat(T![,]);
    p.expect(T![')']);
    m.complete(p, CFG_META);
}

fn finish_attr_meta<S: Sink>(p: &mut Parser<'_, S>, m: Marker) {
    paths::attr_path(p);

    match p.current() {
        T![=] if !p.at(T![=>]) && !p.at(T![==]) => {
            p.bump(T![=]);
            if expressions::expr(p).is_none() {
                p.error("expected expression");
            }
            m.complete(p, KEY_VALUE_META);
        }
        T!['('] | T!['['] | T!['{'] => {
            items::token_tree(p);
            m.complete(p, TOKEN_TREE_META);
        }
        _ => {
            m.complete(p, PATH_META);
        }
    }
}

const META_UNSAFE: u32 = 0;
const META_CFG_ATTR: u32 = 1;

// test metas
// #![simple_ident]
// #![simple::path]
// #![simple_ident_expr = ""]
// #![simple::path::Expr = ""]
// #![simple_ident_tt(a b c)]
// #![simple_ident_tt[a b c]]
// #![simple_ident_tt{a b c}]
// #![simple::path::tt(a b c)]
// #![simple::path::tt[a b c]]
// #![simple::path::tt{a b c}]
// #![unsafe(simple_ident)]
// #![unsafe(simple::path)]
// #![unsafe(simple_ident_expr = "")]
// #![unsafe(simple::path::Expr = "")]
// #![unsafe(simple_ident_tt(a b c))]
// #![unsafe(simple_ident_tt[a b c])]
// #![unsafe(simple_ident_tt{a b c})]
// #![unsafe(simple::path::tt(a b c))]
// #![unsafe(simple::path::tt[a b c])]
// #![unsafe(simple::path::tt{a b c})]
pub(super) fn meta<S: Sink>(p: &mut Parser<'_, S>) {
    // Fast path for non-nested meta items so `KEY_VALUE_META` expressions do not hold a scratch
    // buffer across arbitrary nested attributes.
    if !p.at(T![unsafe]) && !(p.nth_at(1, T!['(']) && p.at_contextual_kw(T![cfg_attr])) {
        let m = p.start();
        if p.nth_at(1, T!['(']) && p.at_contextual_kw(T![cfg]) {
            cfg_meta(p, m);
        } else {
            finish_attr_meta(p, m);
        }
        return;
    }

    blaze_mem::scratch::with_u32s(|stack| {
        let mut close_first = false;
        'parse: loop {
            let m = p.start();
            if p.eat(T![unsafe]) {
                p.expect(T!['(']);
                stack.push(m.into_raw());
                stack.push(META_UNSAFE);
                continue 'parse;
            }

            if p.nth_at(1, T!['(']) {
                if p.at_contextual_kw(T![cfg_attr]) {
                    // test cfg_attr
                    // #![cfg_attr(not(foo), unsafe(bar()), cfg_attr(all(true, foo = "bar"), baz = "baz"))]
                    p.eat_contextual_kw(T![cfg_attr]);
                    p.bump(T!['(']);
                    cfg_predicate(p);
                    p.expect(T![,]);
                    stack.push(m.into_raw());
                    stack.push(META_CFG_ATTR);
                    if !p.at(T![')']) && !p.at(EOF) {
                        continue 'parse;
                    }
                    close_first = true;
                } else if p.at_contextual_kw(T![cfg]) {
                    cfg_meta(p, m);
                } else {
                    finish_attr_meta(p, m);
                }
            } else {
                finish_attr_meta(p, m);
            }

            while let (Some(tag), Some(raw)) = (stack.pop(), stack.pop()) {
                if close_first {
                    close_first = false;
                } else if tag == META_CFG_ATTR
                    && p.eat(T![,])
                    && !p.at(T![')'])
                    && !p.at(EOF)
                {
                    stack.push(raw);
                    stack.push(META_CFG_ATTR);
                    continue 'parse;
                }
                p.expect(T![')']);
                let kind = if tag == META_UNSAFE { UNSAFE_META } else { CFG_ATTR_META };
                Marker::from_raw(raw).complete(p, kind);
            }
            return;
        }
    });
}
