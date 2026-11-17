//! Compile-time layout goldens (docs/hot-reload.md, "Migrations").
//!
//! Every `Layout` and `Component` type is checked against its line in its crate's `layouts.lock` (beside its Cargo.toml) while the crate compiles: size,
//! alignment, every field's name, type text and offset, and every enum constant's value. A mismatch is a compile error that names
//! the type and the crate and says what to do (bump `#[den(version = N)]`, add a migration, run `uv run tools/layouts.py`).
//! `DEN_LAYOUTS_REGEN=1` switches the checks off, so the tool can build a binary that dumps the actual layouts (every type also
//! submits itself to `harness_native_flecs::__private::Golden`'s inventory for that dump).
//!
//! Lock line: `crate::Name v=1 kind=struct size=20 align=4 f=axis@0:FlexAxis|gap@4:f32` (`kind=enum` fields are `Const@value`,
//! `kind=choice` fields are `Variant@0:type`). Types have their whitespace removed.
use proc_macro2::TokenStream as T2;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Lit};

struct Entry {
    version: u32,
    kind: String,
    size: usize,
    align: usize,
    fields: Vec<(String, String, i64)>,
}

fn ty_text(t: &syn::Type) -> String {
    quote!(#t).to_string().split_whitespace().collect()
}

fn parse_den(attrs: &[syn::Attribute], what: &str) -> syn::Result<Option<Lit>> {
    let mut found = None;
    for a in attrs.iter().filter(|a| a.path().is_ident("den")) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident(what) {
                found = Some(m.value()?.parse::<Lit>()?);
            } else if m.path.is_ident("version") || m.path.is_ident("renamed_from") {
                m.value()?.parse::<Lit>()?;
            } else {
                return Err(m.error("unknown den attribute: expected version = N or renamed_from = \"old\""));
            }
            Ok(())
        })?;
    }
    Ok(found)
}

/// The crate's own `layouts.lock`, beside its manifest: a proc macro reads nothing outside its crate's directory (blaze
/// runs it sandboxed to `CARGO_MANIFEST_DIR`), so the goldens are kept per crate, never in a lock above it. None there: the
/// crate's layouts are not checked.
fn lock_path() -> Option<std::path::PathBuf> {
    let f = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?).join("layouts.lock");
    f.is_file().then_some(f)
}

fn parse_line(line: &str) -> Option<(String, Entry)> {
    let mut it = line.split_whitespace();
    let key = it.next()?.to_owned();
    let (mut version, mut kind, mut size, mut align, mut fields) = (1, String::new(), 0, 0, Vec::new());
    for part in it {
        let (k, v) = part.split_once('=')?;
        match k {
            "v" => version = v.parse().ok()?,
            "kind" => kind = v.to_owned(),
            "size" => size = v.parse().ok()?,
            "align" => align = v.parse().ok()?,
            "f" => {
                for f in v.split('|').filter(|f| !f.is_empty()) {
                    let (name, rest) = f.split_once('@')?;
                    let (off, ty) = rest.split_once(':').unwrap_or((rest, ""));
                    fields.push((name.to_owned(), ty.to_owned(), off.parse().ok()?));
                }
            }
            _ => return None,
        }
    }
    Some((key, Entry { version, kind, size, align, fields }))
}

fn golden_of(key: &str) -> Result<Entry, String> {
    let path = lock_path().ok_or("layouts.lock not found above this crate")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(parse_line)
        .find(|(k, _)| k == key)
        .map(|(_, e)| e)
        .ok_or_else(|| "no line in layouts.lock".to_owned())
}

/// `world` for `src/world.rs`, `ui::prefs` for `src/ui/prefs/mod.rs`, empty for `src/lib.rs` and `src/main.rs`: what tells apart two
/// types of one crate with the same name.
fn module_of(file: &str) -> String {
    let f = file.replace('\\', "/");
    let rest = format!("/{f}");
    let rest = rest.split_once("/src/").map_or(f.as_str(), |(_, r)| r);
    let rest = rest.strip_suffix(".rs").unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').filter(|p| !matches!(*p, "lib" | "main" | "mod" | "")).collect();
    parts.join("::")
}

pub fn emit(di: &DeriveInput, file: &str) -> syn::Result<T2> {
    let name = &di.ident;
    let n = name.to_string();
    let version: u32 = match parse_den(&di.attrs, "version")? {
        Some(Lit::Int(i)) => i.base10_parse()?,
        Some(other) => return Err(syn::Error::new_spanned(other, "version must be an integer")),
        None => 1,
    };
    // (name, type text, offset-or-value expression, field tokens for offset_of)
    let (kind, items): (&str, Vec<(String, String, T2)>) = match &di.data {
        Data::Struct(s) => (
            "struct",
            match &s.fields {
                Fields::Named(f) => f
                    .named
                    .iter()
                    .map(|f| {
                        let id = f.ident.as_ref().expect("named");
                        (id.to_string(), ty_text(&f.ty), quote!(::core::mem::offset_of!(#name, #id) as i64))
                    })
                    .collect(),
                Fields::Unnamed(f) => f
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let idx = syn::Index::from(i);
                        (i.to_string(), ty_text(&f.ty), quote!(::core::mem::offset_of!(#name, #idx) as i64))
                    })
                    .collect(),
                Fields::Unit => Vec::new(),
            },
        ),
        Data::Enum(e) if e.variants.iter().all(|v| matches!(v.fields, Fields::Unit)) => {
            ("enum", e.variants.iter().map(|v| { let vi = &v.ident; (vi.to_string(), String::new(), quote!(#name::#vi as i64)) }).collect())
        }
        Data::Enum(e) => (
            "choice",
            e.variants
                .iter()
                .map(|v| {
                    let tys: Vec<String> = v.fields.iter().map(|f| ty_text(&f.ty)).collect();
                    (v.ident.to_string(), tys.join(","), quote!(0i64))
                })
                .collect(),
        ),
        Data::Union(_) => return Ok(T2::new()),
    };
    // Renames: new field name -> old.
    let mut renames: Vec<(String, String)> = Vec::new();
    if let Data::Struct(s) = &di.data {
        for (i, f) in s.fields.iter().enumerate() {
            if let Some(Lit::Str(old)) = parse_den(&f.attrs, "renamed_from")? {
                let new = f.ident.as_ref().map_or_else(|| i.to_string(), ToString::to_string);
                renames.push((new, old.value()));
            }
        }
    }
    let p = quote!(::harness_native_flecs::__private);
    let krate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let module = module_of(file);
    let key = if module.is_empty() { format!("{krate}::{n}") } else { format!("{krate}::{module}::{n}") };
    let fields_data = items.iter().map(|(fname, fty, at)| quote!(#p::GoldenField { name: #fname, ty: #fty, at: #at }));
    let rn = renames.iter().map(|(new, old)| quote!((#new, #old)));
    let submit = quote! {
        #p::inventory::submit! {
            #p::Golden {
                key: #key, krate: env!("CARGO_PKG_NAME"), name: #n, version: #version, kind: #kind,
                size: ::core::mem::size_of::<#name>(), align: ::core::mem::align_of::<#name>(),
                fields: &[#(#fields_data),*], renames: &[#(#rn),*],
            }
        }
    };
    // Types of a crate's tests/, benches/ or examples/ are fixtures: no shipped binary links them, so the lock has no line for them.
    if std::env::var_os("DEN_LAYOUTS_REGEN").is_some() || lock_path().is_none() || !format!("/{}", file.replace('\\', "/")).contains("/src/") {
        return Ok(submit);
    }
    let fix = format!(
        "If the change is intended: bump #[den(version = {version})] on {n} to {}, add a migration for {n} {version} -> {} (harness_native_flecs::migration!), then run `uv run tools/layouts.py` to update layouts.lock",
        version + 1,
        version + 1
    );
    let g = match golden_of(&key) {
        Ok(g) => g,
        Err(why) => {
            let msg = format!("den layout: component {n} ({krate}) has no golden: {why}. Run `uv run tools/layouts.py` to record it");
            return Ok(quote! { compile_error!(#msg); });
        }
    };
    let err = |why: String| -> syn::Result<T2> {
        let msg = format!("den layout: component {n} ({krate}) changed: {why}. {fix}");
        Ok(quote! { compile_error!(#msg); })
    };
    if g.version != version {
        let msg = format!("den layout: component {n} ({krate}) is version {version} in code but version {} in layouts.lock: run `uv run tools/layouts.py` after the migration {} -> {version} exists", g.version, g.version);
        return Ok(quote! { compile_error!(#msg); });
    }
    if g.kind != kind {
        return err(format!("it was a {}, it is a {kind}", g.kind));
    }
    let cur: Vec<(&str, &str)> = items.iter().map(|(a, b, _)| (a.as_str(), b.as_str())).collect();
    let old: Vec<(&str, &str)> = g.fields.iter().map(|(a, b, _)| (a.as_str(), b.as_str())).collect();
    if cur != old {
        let show = |v: &[(&str, &str)]| v.iter().map(|(a, b)| if b.is_empty() { (*a).to_owned() } else { format!("{a}: {b}") }).collect::<Vec<_>>().join(", ");
        return err(format!("fields were [{}], now [{}]", show(&old), show(&cur)));
    }
    let mut asserts = Vec::new();
    let (gs, ga) = (g.size, g.align);
    let m1 = format!("den layout: component {n} ({krate}) changed: size is no longer {gs}. {fix}");
    let m2 = format!("den layout: component {n} ({krate}) changed: alignment is no longer {ga}. {fix}");
    asserts.push(quote!(assert!(::core::mem::size_of::<#name>() == #gs, #m1);));
    asserts.push(quote!(assert!(::core::mem::align_of::<#name>() == #ga, #m2);));
    if kind != "choice" {
        for ((fname, _, at), (_, _, want)) in items.iter().zip(&g.fields) {
            let what = if kind == "enum" { "value of constant" } else { "offset of field" };
            let m = format!("den layout: component {n} ({krate}) changed: {what} `{fname}` is no longer {want}. {fix}");
            asserts.push(quote!(assert!(#at == #want, #m);));
        }
    }
    let track = lock_path().map(|f| f.to_string_lossy().into_owned()).map(|f| quote!(const _: &[u8] = include_bytes!(#f);));
    Ok(quote! {
        #submit
        #track
        const _: () = { #(#asserts)* };
    })
}
