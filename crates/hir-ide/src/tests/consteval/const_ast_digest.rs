//! What a derive reads of a file through `const_ast`'s views, folded into one number: every item's
//! name, attributes, visibility and generics, and every struct's, enum's and union's fields,
//! variants and types, down to their tokens and spans. The `const_ast` test runs this in the
//! const evaluator and compiled, on the same tree, and compares the two.

use const_ast::*;

const P: u128 = (1 << 61) - 1;

const fn mix(h: u64, x: u64) -> u64 {
    ((h as u128 * 1_000_003 + x as u128) % P) as u64
}

const fn text(mut h: u64, text: &str) -> u64 {
    let bytes = text.as_bytes();
    h = mix(h, bytes.len() as u64);
    let mut i = 0;
    while i < bytes.len() {
        h = mix(h, bytes[i] as u64);
        i += 1;
    }
    h
}

const fn tokens(mut h: u64, node: Node<'_>) -> u64 {
    h = mix(h, node.index() as u64);
    h = mix(h, node.len() as u64);
    let mut tokens = node.tokens();
    while let Some(token) = tokens.next() {
        h = text(h, token.text());
        h = mix(h, token.span() as u64);
    }
    h
}

const fn name(h: u64, name: Option<Name<'_>>) -> u64 {
    match name {
        Some(it) => text(mix(h, 1), it.text()),
        None => mix(h, 2),
    }
}

const fn path(mut h: u64, path: Option<Path<'_>>) -> u64 {
    let Some(path) = path else { return mix(h, 3) };
    h = self::path(h, path.qualifier());
    if let Some(segment) = path.segment() {
        if let Some(name) = segment.name_ref() {
            h = text(h, name.text());
        }
        if let Some(args) = segment.generic_arg_list() {
            let mut args = args.generic_args();
            while let Some(arg) = args.next() {
                h = ty(h, arg.as_type());
                if let Some(lifetime) = arg.as_lifetime() {
                    h = text(h, lifetime.text());
                }
                if let Some(it) = arg.as_const() {
                    h = tokens(h, it.syntax());
                }
            }
        }
    }
    if let Some(name) = path.as_single_name_ref() {
        h = text(mix(h, 4), name.text());
    }
    h
}

const fn ty(mut h: u64, ty: Option<Type<'_>>) -> u64 {
    let Some(ty) = ty else { return mix(h, 5) };
    h = tokens(h, ty.syntax());
    if let Some(it) = ty.as_path() {
        h = path(mix(h, 6), it.path());
    } else if let Some(it) = ty.as_ref() {
        h = mix(h, 7 + it.is_mut() as u64);
        if let Some(lifetime) = it.lifetime() {
            h = text(h, lifetime.text());
        }
        h = self::ty(h, it.ty());
    } else if let Some(it) = ty.as_ptr() {
        h = mix(h, 9 + it.is_mut() as u64 + 2 * it.is_const() as u64);
        h = self::ty(h, it.ty());
    } else if let Some(it) = ty.as_tuple() {
        let mut fields = it.fields();
        while let Some(field) = fields.next() {
            h = self::ty(mix(h, 13), Some(field));
        }
        h = self::ty(h, it.field(1));
    } else if let Some(it) = ty.as_array() {
        h = self::ty(mix(h, 14), it.ty());
        if let Some(len) = it.const_arg() {
            h = tokens(h, len.syntax());
        }
    } else if let Some(it) = ty.as_slice() {
        h = self::ty(mix(h, 15), it.ty());
    }
    h
}

const fn attrs(mut h: u64, mut attrs: Attrs<'_>) -> u64 {
    while let Some(attr) = attrs.next() {
        h = mix(h, 16 + attr.is_inner() as u64);
        h = path(h, attr.path());
        if let Some(name) = attr.simple_name() {
            h = text(h, name);
        }
        if let Some((name, args)) = attr.as_simple_call() {
            h = tokens(text(h, name), args.syntax());
        }
        if let Some(meta) = attr.meta() {
            h = mix(h, meta.is_cfg() as u64 + 2 * meta.is_cfg_attr() as u64);
            h = mix(h, meta.is_key_value() as u64 + 2 * meta.unsafe_meta().is_some() as u64);
            if let Some(atom) = meta.as_simple_atom() {
                h = text(h, atom);
            }
            if let Some(tt) = meta.token_tree() {
                h = tokens(h, tt.syntax());
            }
        }
    }
    h
}

const fn visibility(h: u64, vis: Option<Visibility<'_>>) -> u64 {
    match vis {
        Some(vis) => match vis.visibility_inner() {
            Some(inner) => path(mix(h, 18), inner.path()),
            None => mix(h, 19),
        },
        None => mix(h, 20),
    }
}

const fn generics(
    mut h: u64,
    params: Option<GenericParamList<'_>>,
    clause: Option<WhereClause<'_>>,
) -> u64 {
    if let Some(params) = params {
        let mut params = params.generic_params();
        while let Some(param) = params.next() {
            match param {
                GenericParam::TypeParam(it) => {
                    h = attrs(name(mix(h, 21), it.name()), it.attrs());
                    if let Some(bounds) = it.type_bound_list() {
                        h = tokens(h, bounds.syntax());
                    }
                    h = ty(h, it.default_type());
                }
                GenericParam::LifetimeParam(it) => {
                    h = attrs(mix(h, 22), it.attrs());
                    if let Some(lifetime) = it.lifetime() {
                        h = text(h, lifetime.text());
                    }
                    if let Some(bounds) = it.type_bound_list() {
                        h = tokens(h, bounds.syntax());
                    }
                }
                GenericParam::ConstParam(it) => {
                    h = attrs(name(mix(h, 23), it.name()), it.attrs());
                    h = ty(h, it.ty());
                    if let Some(default) = it.default_val() {
                        h = tokens(h, default.syntax());
                    }
                }
            }
        }
    }
    if let Some(clause) = clause {
        let mut preds = clause.predicates();
        while let Some(pred) = preds.next() {
            h = ty(mix(h, 24), pred.ty());
            if let Some(lifetime) = pred.lifetime() {
                h = text(h, lifetime.text());
            }
            if let Some(bounds) = pred.type_bound_list() {
                h = tokens(h, bounds.syntax());
            }
        }
    }
    h
}

const fn fields(mut h: u64, list: Option<FieldList<'_>>) -> u64 {
    match list {
        Some(FieldList::RecordFieldList(list)) => {
            let mut fields = list.fields();
            h = mix(h, list.fields().count() as u64);
            while let Some(field) = fields.next() {
                h = name(mix(h, 25), field.name());
                h = attrs(h, field.attrs());
                h = visibility(h, field.visibility());
                h = ty(h, field.ty());
                if let Some(default) = field.default_val() {
                    h = tokens(h, default.syntax());
                }
            }
        }
        Some(FieldList::TupleFieldList(list)) => {
            let mut fields = list.fields();
            while let Some(field) = fields.next() {
                h = attrs(mix(h, 26), field.attrs());
                h = visibility(h, field.visibility());
                h = ty(h, field.ty());
            }
        }
        None => h = mix(h, 27),
    }
    h
}

const fn item(mut h: u64, item: Item<'_>) -> u64 {
    h = mix(h, item.syntax().index() as u64);
    h = name(h, item.name());
    h = attrs(h, item.attrs());
    h = visibility(h, item.visibility());
    h = generics(h, item.generic_param_list(), item.where_clause());
    if let Some(it) = item.as_struct() {
        h = fields(mix(h, 28), it.field_list());
    } else if let Some(it) = item.as_enum() {
        if let Some(list) = it.variant_list() {
            let mut variants = list.variants();
            while let Some(variant) = variants.next() {
                h = name(mix(h, 29), variant.name());
                h = attrs(h, variant.attrs());
                h = fields(h, variant.field_list());
                if let Some(value) = variant.const_arg() {
                    h = tokens(h, value.syntax());
                }
            }
        }
    } else if let Some(it) = item.as_union() {
        if let Some(list) = it.record_field_list() {
            h = fields(mix(h, 30), Some(FieldList::RecordFieldList(list)));
        }
    }
    if let Some(adt) = item.as_adt() {
        h = name(mix(h, 31), adt.name());
    }
    h
}

/// The digest of every item of the file at `tree`'s root.
pub const fn digest(tree: &Tree<'_>) -> u64 {
    let Some(file) = SourceFile::cast(tree.root()) else { return 0 };
    let mut h = attrs(0, file.attrs());
    let mut items = file.items();
    while let Some(it) = items.next() {
        h = item(h, it);
    }
    h
}
