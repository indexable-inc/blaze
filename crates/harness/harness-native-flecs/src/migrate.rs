//! Component schema versions and migrations (docs/hot-reload.md, "Migrations").
//!
//! Every `Layout` type records a [`Golden`] (its version, size, alignment, fields) in an inventory; `layouts.lock` is the
//! committed copy and the derive checks it at compile time. When a layout changes, the component's version is bumped and a
//! [`Migration`] for that step is registered (`harness_native_flecs::migration!`). Converting a value is then:
//!
//! 1. [`convert`]: field by field from the old recipe to the new one. A field present in both (or renamed with
//!    `#[den(renamed_from = "old")]`) is moved (same type) or widened (number to a wider or float number); a field only the new
//!    type has keeps its `Default`; a field only the old type had is dropped and reported. A type that cannot be converted
//!    (a string that became a number) is a [`Problem`], never a panic.
//! 2. the registered step for the version change, if any, which sees the old value by field name and may set the new one.
//!
//! [`convert_all`] converts every instance first and returns them only if all succeeded, so a caller commits all or none.
//! [`convert_json`] is the same for values read as JSON (scene data, saved state).
use hyperion_layout::layout::{Field, Prim, Recipe};
use serde_json::{Map, Value};

/// One field (or enum constant, or choice variant) of a type's golden. `at` is the offset, or the constant's value.
#[derive(Debug)]
pub struct GoldenField {
    pub name: &'static str,
    pub ty: &'static str,
    pub at: i64,
}

/// A layout type's recorded shape, submitted by the derive (the source of `layouts.lock`).
#[derive(Debug)]
pub struct Golden {
    /// `crate::Name`.
    pub key: &'static str,
    pub krate: &'static str,
    pub name: &'static str,
    pub version: u32,
    pub kind: &'static str,
    pub size: usize,
    pub align: usize,
    pub fields: &'static [GoldenField],
    /// `(new field, old field)` from `#[den(renamed_from = "old")]`.
    pub renames: &'static [(&'static str, &'static str)],
}

inventory::collect!(Golden);

/// Every layout type compiled into this image.
pub fn goldens() -> impl Iterator<Item = &'static Golden> {
    inventory::iter::<Golden>.into_iter()
}

/// The golden of the type named `name` (the world name, the type's name) in api crate `krate`, if there is exactly that one.
#[must_use]
pub fn golden_of(krate: &str, name: &str) -> Option<&'static Golden> {
    goldens().find(|g| g.name == name && g.krate.replace('-', "_") == krate.replace('-', "_"))
}

/// A hand-written conversion for one version step of a component, run after the automatic field mapping.
pub struct Migration {
    /// The component's type name.
    pub component: &'static str,
    /// The api crate (`CARGO_PKG_NAME`) that owns it.
    pub krate: &'static str,
    pub from: u32,
    pub to: u32,
    pub step: fn(&OldValue<'_>, &mut NewValue<'_>) -> Result<(), String>,
}

inventory::collect!(Migration);

/// The registered steps of `component`, lowest `from` first.
#[must_use]
pub fn steps_of(component: &str) -> Vec<&'static Migration> {
    let mut v: Vec<_> = inventory::iter::<Migration>.into_iter().filter(|m| m.component == component).collect();
    v.sort_by_key(|m| m.from);
    v
}

/// Register a migration step of component `$ty` from version `$from` to `$to`. `$f` is `|old, new| -> Result<(), String>`;
/// `old.get::<f32>("gap")` reads an old field by name, `new.set("spacing", v)` writes a new one.
///
/// ```ignore
/// harness_native_flecs::migration!(Flex, 1 => 2, |old, new| {
///     new.set("gap", old.get::<f32>("spacing").ok_or("no spacing")? * 2.0)
/// });
/// ```
#[macro_export]
macro_rules! migration {
    ($ty:ident, $from:literal => $to:literal, $f:expr) => {
        $crate::__private::inventory::submit! {
            $crate::migrate::Migration {
                component: stringify!($ty), krate: env!("CARGO_PKG_NAME"), from: $from, to: $to,
                step: $f,
            }
        }
    };
}

/// Something went wrong converting a value, in words for the person who changed the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem(pub String);

impl core::fmt::Display for Problem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Problem {}

/// What converting did that a person may want to know.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// `(old, new)` field names matched through `renamed_from`.
    pub renamed: Vec<(String, String)>,
    /// Fields the new type has and the old did not: they keep their default.
    pub defaulted: Vec<String>,
    /// Fields the old type had and the new does not: their values are gone.
    pub dropped: Vec<String>,
    /// Fields whose number type changed (`gap: f32 -> f64`).
    pub widened: Vec<String>,
}

impl Report {
    fn absorb(&mut self, prefix: &str, other: Report) {
        let p = |s: String| if prefix.is_empty() { s } else { format!("{prefix}.{s}") };
        self.renamed.extend(other.renamed.into_iter().map(|(a, b)| (p(a), p(b))));
        self.defaulted.extend(other.defaulted.into_iter().map(p));
        self.dropped.extend(other.dropped.into_iter().map(p));
        self.widened.extend(other.widened.into_iter().map(p));
    }

    /// One line, empty when nothing needed saying.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.renamed.is_empty() {
            parts.push(format!("renamed {}", self.renamed.iter().map(|(a, b)| format!("{a} -> {b}")).collect::<Vec<_>>().join(", ")));
        }
        if !self.defaulted.is_empty() {
            parts.push(format!("new, defaulted: {}", self.defaulted.join(", ")));
        }
        if !self.dropped.is_empty() {
            parts.push(format!("dropped: {}", self.dropped.join(", ")));
        }
        if !self.widened.is_empty() {
            parts.push(format!("widened: {}", self.widened.join(", ")));
        }
        parts.join("; ")
    }
}

fn problem(s: String) -> Problem {
    Problem(s)
}

fn prim_name(r: &Recipe) -> String {
    match r {
        Recipe::Prim { name, .. } => (*name).to_owned(),
        Recipe::Array { len, elem, .. } => format!("[{}; {len}]", prim_name(elem)),
        Recipe::Struct { .. } => "a record".to_owned(),
        Recipe::Enum { .. } => "a choice of named values".to_owned(),
        Recipe::Opaque { shape, .. } => prim_name(shape),
    }
}

fn read_num(kind: Prim, b: &[u8]) -> Option<f64> {
    Some(match kind {
        Prim::Bool => f64::from(u8::from(b[0] != 0)),
        Prim::U8 => f64::from(b[0]),
        Prim::U16 => f64::from(u16::from_ne_bytes(b[..2].try_into().ok()?)),
        Prim::U32 => f64::from(u32::from_ne_bytes(b[..4].try_into().ok()?)),
        Prim::U64 => u64::from_ne_bytes(b[..8].try_into().ok()?) as f64,
        Prim::I8 => f64::from(b[0] as i8),
        Prim::I16 => f64::from(i16::from_ne_bytes(b[..2].try_into().ok()?)),
        Prim::I32 => f64::from(i32::from_ne_bytes(b[..4].try_into().ok()?)),
        Prim::I64 => i64::from_ne_bytes(b[..8].try_into().ok()?) as f64,
        Prim::F32 => f64::from(f32::from_ne_bytes(b[..4].try_into().ok()?)),
        Prim::F64 => f64::from_ne_bytes(b[..8].try_into().ok()?),
        Prim::Ptr | Prim::Str => return None,
    })
}

fn write_num(kind: Prim, v: f64, out: &mut [u8]) -> bool {
    let int = |lo: f64, hi: f64| v.fract() == 0.0 && v >= lo && v <= hi;
    match kind {
        Prim::U8 if int(0.0, 255.0) => out[0] = v as u8,
        Prim::U16 if int(0.0, 65535.0) => out[..2].copy_from_slice(&(v as u16).to_ne_bytes()),
        Prim::U32 if int(0.0, 4_294_967_295.0) => out[..4].copy_from_slice(&(v as u32).to_ne_bytes()),
        Prim::U64 if int(0.0, 1.8e19) => out[..8].copy_from_slice(&(v as u64).to_ne_bytes()),
        Prim::I8 if int(-128.0, 127.0) => out[0] = (v as i8) as u8,
        Prim::I16 if int(-32768.0, 32767.0) => out[..2].copy_from_slice(&(v as i16).to_ne_bytes()),
        Prim::I32 if int(-2_147_483_648.0, 2_147_483_647.0) => out[..4].copy_from_slice(&(v as i32).to_ne_bytes()),
        Prim::I64 if int(-9.2e18, 9.2e18) => out[..8].copy_from_slice(&(v as i64).to_ne_bytes()),
        Prim::F32 => out[..4].copy_from_slice(&(v as f32).to_ne_bytes()),
        Prim::F64 => out[..8].copy_from_slice(&v.to_ne_bytes()),
        Prim::Bool => out[0] = u8::from(v != 0.0),
        _ => return false,
    }
    true
}

fn prim_kind(r: &Recipe) -> Option<Prim> {
    match r {
        Recipe::Prim { kind, .. } if !matches!(kind, Prim::Ptr | Prim::Str) => Some(*kind),
        _ => None,
    }
}

fn struct_fields(r: &Recipe) -> Option<&'static [Field]> {
    match r {
        Recipe::Struct { fields, .. } => Some(fields),
        _ => None,
    }
}

fn convert_value(path: &str, old: &Recipe, new: &Recipe, ob: &[u8], nb: &mut [u8], renames: &[(&str, &str)], report: &mut Report) -> Result<(), Problem> {
    if old.hash() == new.hash() {
        nb[..new.size()].copy_from_slice(&ob[..old.size()]);
        return Ok(());
    }
    if let (Some(a), Some(b)) = (prim_kind(old), prim_kind(new)) {
        let v = read_num(a, ob).ok_or_else(|| problem(format!("{path}: cannot read the old value")))?;
        if !write_num(b, v, nb) {
            return Err(problem(format!("{path}: the old value {v} does not fit {}", prim_name(new))));
        }
        report.widened.push(format!("{path}: {} -> {}", prim_name(old), prim_name(new)));
        return Ok(());
    }
    if let (Some(of), Some(nf)) = (struct_fields(old), struct_fields(new)) {
        let nested = renames;
        let mut inner = Report::default();
        for n in nf {
            let renamed = nested.iter().find(|(new_name, _)| *new_name == n.name).map(|(_, o)| *o);
            let found = of.iter().find(|o| o.name == n.name).map(|o| (o, false)).or_else(|| renamed.and_then(|r| of.iter().find(|o| o.name == r).map(|o| (o, true))));
            match found {
                Some((o, was_renamed)) => {
                    if was_renamed {
                        inner.renamed.push((o.name.to_owned(), n.name.to_owned()));
                    }
                    let mut sub = Report::default();
                    convert_value(n.name, o.ty, n.ty, &ob[o.offset..o.offset + o.ty.size()], &mut nb[n.offset..n.offset + n.ty.size()], &[], &mut sub)
                        .map_err(|e| problem(if path.is_empty() { e.0 } else { format!("{path}.{}", e.0) }))?;
                    inner.absorb(n.name, sub);
                }
                None => inner.defaulted.push(n.name.to_owned()),
            }
        }
        for o in of {
            let kept = nf.iter().any(|n| n.name == o.name) || nested.iter().any(|(new_name, old_name)| *old_name == o.name && nf.iter().any(|n| n.name == *new_name));
            if !kept {
                inner.dropped.push(o.name.to_owned());
            }
        }
        report.absorb("", inner);
        return Ok(());
    }
    if let (Recipe::Enum { consts: oc, size: os, .. }, Recipe::Enum { consts: nc, tag, .. }) = (old, new) {
        let tag_kind = prim_kind(tag).ok_or_else(|| problem(format!("{path}: a choice with an unreadable tag")))?;
        let value = read_num(prim_for_size(*os), ob).unwrap_or(0.0) as i64;
        let name = oc.iter().find(|(_, v)| *v == value).map(|(n, _)| *n).ok_or_else(|| problem(format!("{path}: the old value {value} is not a known choice")))?;
        let to = nc.iter().find(|(n, _)| *n == name).ok_or_else(|| problem(format!("{path}: the choice `{name}` no longer exists")))?;
        write_num(tag_kind, to.1 as f64, nb);
        return Ok(());
    }
    Err(problem(format!("{path}: was {}, is now {}: add a migration step for this field", prim_name(old), prim_name(new))))
}

fn prim_for_size(size: usize) -> Prim {
    match size {
        1 => Prim::U8,
        2 => Prim::U16,
        4 => Prim::U32,
        _ => Prim::U64,
    }
}

/// A value of the old layout, read by field name.
pub struct OldValue<'a> {
    pub recipe: &'static Recipe,
    pub bytes: &'a [u8],
}

impl OldValue<'_> {
    /// The old field `name` as a `T` (a number, `bool`): `None` when the old type has no such field or it is another type.
    #[must_use]
    pub fn get<T: Copy + 'static>(&self, name: &str) -> Option<T> {
        let f = struct_fields(self.recipe)?.iter().find(|f| f.name == name)?;
        (f.ty.size() == size_of::<T>()).then(|| {
            // SAFETY: the field's recipe says `size_of::<T>()` bytes live at its offset inside `bytes`; `T` is plain data.
            unsafe { self.bytes.as_ptr().add(f.offset).cast::<T>().read_unaligned() }
        })
    }
}

/// The value of the new layout being built (it starts as the type's default).
pub struct NewValue<'a> {
    pub recipe: &'static Recipe,
    pub bytes: &'a mut [u8],
}

impl NewValue<'_> {
    /// Set the new field `name` to `v`, which must be the field's size.
    pub fn set<T: Copy + 'static>(&mut self, name: &str, v: T) -> Result<(), String> {
        let f = struct_fields(self.recipe).and_then(|fs| fs.iter().find(|f| f.name == name)).ok_or_else(|| format!("the new type has no field {name}"))?;
        if f.ty.size() != size_of::<T>() {
            return Err(format!("field {name} is {} bytes, not {}", f.ty.size(), size_of::<T>()));
        }
        // SAFETY: bounds are the field's own; `T` is plain data.
        unsafe { self.bytes.as_mut_ptr().add(f.offset).cast::<T>().write_unaligned(v) };
        Ok(())
    }
}

/// Convert one value from `old` to `new`. `default` is the new type's default value (its bytes), the base the fields only
/// the new type has keep. `steps` run last, in order.
///
/// # Errors
/// A [`Problem`] naming the field when a type cannot be mapped or a step fails.
pub fn convert(old: &'static Recipe, new: &'static Recipe, renames: &[(&str, &str)], steps: &[&Migration], default: &[u8], bytes: &[u8]) -> Result<(Vec<u8>, Report), Problem> {
    if bytes.len() < old.size() || default.len() < new.size() {
        return Err(problem(format!("a value of {} bytes, the old layout is {}", bytes.len(), old.size())));
    }
    let mut out = default[..new.size()].to_vec();
    let mut report = Report::default();
    convert_value("", old, new, bytes, &mut out, renames, &mut report)?;
    for s in steps {
        (s.step)(&OldValue { recipe: old, bytes }, &mut NewValue { recipe: new, bytes: &mut out }).map_err(|e| problem(format!("{} migration {} -> {}: {e}", s.component, s.from, s.to)))?;
    }
    Ok((out, report))
}

/// Convert every instance, returning all of them or the first problem: nothing is half done.
///
/// # Errors
/// The first [`Problem`], with the entity index it happened on.
pub fn convert_all<E: Copy>(
    old: &'static Recipe,
    new: &'static Recipe,
    renames: &[(&str, &str)],
    steps: &[&Migration],
    default: &[u8],
    instances: &[(E, Vec<u8>)],
) -> Result<(Vec<(E, Vec<u8>)>, Report), Problem> {
    let mut out = Vec::with_capacity(instances.len());
    let mut report = Report::default();
    for (i, (e, bytes)) in instances.iter().enumerate() {
        let (b, r) = convert(old, new, renames, steps, default, bytes).map_err(|p| problem(format!("instance {i}: {p}")))?;
        if i == 0 {
            report = r;
        }
        out.push((*e, b));
    }
    Ok((out, report))
}

fn json_kind(v: &Value, r: &Recipe) -> bool {
    match r {
        Recipe::Prim { kind: Prim::Bool, .. } => v.is_boolean(),
        Recipe::Prim { kind: Prim::Str | Prim::Ptr, .. } => v.is_string() || v.is_null(),
        Recipe::Prim { .. } => v.is_number(),
        Recipe::Array { len, elem, .. } => v.as_array().is_some_and(|a| a.len() == *len && a.iter().all(|x| json_kind(x, elem))),
        Recipe::Struct { .. } => v.is_object(),
        Recipe::Enum { consts, .. } => v.as_str().is_some_and(|s| consts.iter().any(|(n, _)| *n == s)) || v.is_number(),
        Recipe::Opaque { .. } => true,
    }
}

fn check_obj(path: &str, obj: &mut Map<String, Value>, fields: &'static [Field], renames: &[(&str, &str)], report: &mut Report) -> Result<(), Problem> {
    for (new_name, old_name) in renames {
        if !obj.contains_key(*new_name)
            && let Some(v) = obj.remove(*old_name)
        {
            obj.insert((*new_name).to_owned(), v);
            report.renamed.push(((*old_name).to_owned(), (*new_name).to_owned()));
        }
    }
    let unknown: Vec<String> = obj.keys().filter(|k| !fields.iter().any(|f| f.name == k.as_str())).cloned().collect();
    for k in unknown {
        obj.remove(&k);
        report.dropped.push(k);
    }
    for f in fields {
        let here = if path.is_empty() { f.name.to_owned() } else { format!("{path}.{}", f.name) };
        match obj.get_mut(f.name) {
            None => report.defaulted.push(here),
            Some(v) => {
                if !json_kind(v, f.ty) {
                    return Err(problem(format!("{here}: expected {}, found {}", prim_name(f.ty), json_word(v))));
                }
                if let (Value::Object(inner), Some(fs)) = (v, struct_fields(f.ty)) {
                    check_obj(&here, inner, fs, &[], report)?;
                }
            }
        }
    }
    Ok(())
}

fn json_word(v: &Value) -> &'static str {
    match v {
        Value::Null => "nothing",
        Value::Bool(_) => "a yes/no",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "a record",
    }
}

/// Bring a component value read as JSON (a scene or saved state) to the current layout: fields renamed with `renamed_from` are
/// moved, fields the type no longer has are dropped (listed in the report, for a warning), missing fields are left out so they
/// take their default, and a value of the wrong type is a [`Problem`] naming the field.
///
/// # Errors
/// A [`Problem`] when `value` is not a record or a field has the wrong type.
pub fn convert_json(recipe: &'static Recipe, renames: &[(&str, &str)], value: &mut Value) -> Result<Report, Problem> {
    let (Some(obj), Some(fields)) = (value.as_object_mut(), struct_fields(recipe)) else {
        return Err(problem(format!("expected a record of {}, found {}", prim_name(recipe), json_word(value))));
    };
    let mut report = Report::default();
    check_obj("", obj, fields, renames, &mut report)?;
    Ok(report)
}

/// The lines of `layouts.lock` for every layout type compiled into this image, sorted, then one `migration:Name:from:to` line per
/// registered step. `tools/layouts.py` writes the lock from `den-native --dump-layouts`, which prints this.
///
/// # Errors
/// Two types of one crate with the same name (the lock keys by `crate::Name`).
pub fn lock_lines() -> Result<Vec<String>, String> {
    let mut lines: Vec<(String, String)> = Vec::new();
    for g in goldens() {
        let fields: Vec<String> = g.fields.iter().map(|f| if f.ty.is_empty() { format!("{}@{}", f.name, f.at) } else { format!("{}@{}:{}", f.name, f.at, f.ty) }).collect();
        let line = format!("{} v={} kind={} size={} align={} f={}", g.key, g.version, g.kind, g.size, g.align, fields.join("|"));
        if lines.iter().any(|(k, _)| k == g.key) {
            return Err(format!("two layout types named {}: the lock keys by crate and type name, rename one", g.key));
        }
        lines.push((g.key.to_owned(), line));
    }
    lines.sort();
    let mut out: Vec<String> = lines.into_iter().map(|(_, l)| l).collect();
    let mut steps: Vec<String> = inventory::iter::<Migration>.into_iter().map(|m| format!("migration:{}:{}:{}", m.component, m.from, m.to)).collect();
    steps.sort();
    out.extend(steps);
    Ok(out)
}
