//! Explicit strict vs. lossy decoding, downgrade-safe unknown-field preservation, finite version
//! dispatch, and per-item list fault isolation.
//!
//! Reading a [`Value`] in `blaze-data` is strict by default ([`decode_strict`], [`Cursor::fields`]).
//! When a caller intentionally opts into forward- or backward-compatible decoding (such as reading
//! a config or session written by a newer or older schema version), [`decode_lossy`] and
//! [`Cursor::fields_lossy`] return a [`Decoded`] that **explicitly** surfaces:
//! - every unconsumed key and its exact [`Value`] in [`Decoded::unknown`] (structured as a
//!   preserved sub-map so [`merge_unknown_into`] can re-emit it losslessly on save without
//!   overwriting current fields), and
//! - every missing or null defaulted field's exact dotted path in [`Decoded::defaulted`].
//!
//! Nothing is silently dropped or coerced: missing required fields and wrong value kinds are
//! always refused with a [`ShapeError`] naming the path.

#![blaze::sans_io]

use core::{alloc::Allocator, cell::RefCell, fmt, fmt::Write as _};

use blaze_own::{
    String, Vec,
    planted::{NoPlant, Planted, Plants},
};

use crate::{
    Cursor, FromValue, Map, ShapeError, Step, Value,
    path::closest,
    write_step,
};

/// Bugs a control can plant in lossy decoding and unknown-field merging (rule 53; D165): shipped
/// code passes [`NoPlant`], which compiles every check away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LossyPlant {
    /// Lossy decoding as shipped.
    None,
    /// Silently drops unknown top-level keys instead of capturing them in [`Decoded::unknown`].
    DropUnknownFields,
    /// Silently drops a recorded defaulted field path from [`Decoded::defaulted`].
    DropDefaultedPath,
    /// Overwrites an existing key in [`merge_unknown_into`] instead of preserving current fields.
    OverwriteOnMerge,
}

impl Plants for LossyPlant {}

/// The explicit result of lossy decoding: the typed `value`, every unconsumed key with its exact
/// [`Value`] preserved for inspection or downgrade-safe re-serialization, and the exact dotted
/// paths of every field filled from a default.
#[must_use = "inspect `unknown` and `defaulted`, or unpack `.value` explicitly"]
pub struct Decoded<T, A: Allocator> {
    pub value: T,
    /// Unknown keys encountered, mirroring the document's nested map structure so callers can both
    /// inspect what was ignored and pass it directly to [`merge_unknown_into`] on save.
    pub unknown: Map<A>,
    /// Exact dotted paths (e.g. `a.b.theme`) of fields that were missing or null and filled from
    /// defaults.
    pub defaulted: Vec<String<A>, A>,
}

impl<T, A: Allocator> Decoded<T, A> {
    /// Constructs a [`Decoded`] with no unknown keys and no defaulted fields.
    pub fn exact(value: T, alloc: A) -> Decoded<T, A>
    where
        A: Clone,
    {
        Decoded {
            value,
            unknown: Map::new_in(alloc.clone()),
            defaulted: Vec::new_in(alloc),
        }
    }

    /// `true` when every key in the input was consumed and no field fell back to a default.
    pub fn is_exact(&self) -> bool {
        self.unknown.is_empty() && self.defaulted.is_empty()
    }

    /// `true` when at least one unknown key was preserved in [`Self::unknown`].
    pub fn has_unknown(&self) -> bool {
        !self.unknown.is_empty()
    }

    /// `true` when at least one field was missing or null and filled from a default.
    pub fn has_defaulted(&self) -> bool {
        !self.defaulted.is_empty()
    }

    /// Maps the decoded `value` while preserving `unknown` and `defaulted` untouched.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Decoded<U, A> {
        Decoded {
            value: f(self.value),
            unknown: self.unknown,
            defaulted: self.defaulted,
        }
    }

    /// Dotted paths of every unknown leaf (or empty unknown map) in [`Self::unknown`], in
    /// insertion order, formatted with [`write_step`] (the same path syntax as [`ShapeError`]).
    /// Walks iteratively with a stack in `alloc` so arbitrary nesting cannot overflow the stack.
    pub fn unknown_paths(&self, alloc: A) -> Vec<String<A>, A>
    where
        A: Clone,
    {
        let mut out = Vec::new_in(alloc.clone());
        if self.unknown.is_empty() {
            return out;
        }
        let mut path = String::new_in(alloc.clone());
        let mut stack: Vec<(&Map<A>, usize, usize), A> = Vec::new_in(alloc.clone());
        stack.push((&self.unknown, 0, 0));
        while let Some((map, idx, saved_len)) = stack.last_mut() {
            if let Some((key, val)) = map.entry_at(*idx) {
                *idx += 1;
                let before_len = path.len();
                let has_prefix = before_len > 0;
                if write_step(&mut path, Step::Key(key), has_prefix).is_err() {
                    unreachable!("writing into a `String` does not fail");
                }
                if let Value::Map(sub) = val {
                    if !sub.is_empty() {
                        stack.push((sub, 0, before_len));
                        continue;
                    }
                }
                out.push(String::from_str_in(path.as_str(), alloc.clone()));
                path.truncate(before_len);
            } else {
                let restore = *saved_len;
                stack.pop();
                path.truncate(restore);
            }
        }
        out
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for Decoded<T, A> {
    fn clone(&self) -> Decoded<T, A> {
        Decoded {
            value: self.value.clone(),
            unknown: self.unknown.clone(),
            defaulted: self.defaulted.clone(),
        }
    }
}

impl<T: PartialEq, A: Allocator> PartialEq for Decoded<T, A> {
    fn eq(&self, other: &Decoded<T, A>) -> bool {
        self.value == other.value
            && self.unknown == other.unknown
            && self.defaulted.len() == other.defaulted.len()
            && self
                .defaulted
                .iter()
                .zip(other.defaulted.iter())
                .all(|(a, b)| a.as_str() == b.as_str())
    }
}

impl<T: Eq, A: Allocator> Eq for Decoded<T, A> {}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Decoded<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoded")
            .field("value", &self.value)
            .field("unknown", &self.unknown)
            .field("defaulted", &self.defaulted)
            .finish()
    }
}

/// A type read lossily from the value under a [`Cursor`], explicitly returning every unknown field
/// and every defaulted field path alongside the decoded value.
pub trait FromValueLossy<A: Allocator + Clone>: Sized {
    fn from_value_lossy(cursor: &Cursor<'_, '_, A>) -> Result<Decoded<Self, A>, ShapeError<A>>;
}

/// Reads `T` strictly: every unknown key, wrong kind, out-of-range integer, or unknown variant is
/// refused with a [`ShapeError`] naming the path.
pub fn decode_strict<T: FromValue<A>, A: Allocator + Clone>(
    cursor: &Cursor<'_, '_, A>,
) -> Result<T, ShapeError<A>> {
    T::from_value(cursor)
}

/// Reads `T` lossily: returns a [`Decoded`] that surfaces every unconsumed key (with its exact
/// [`Value`]) and every missing/null field filled from a default.
pub fn decode_lossy<T: FromValueLossy<A>, A: Allocator + Clone>(
    cursor: &Cursor<'_, '_, A>,
) -> Result<Decoded<T, A>, ShapeError<A>> {
    T::from_value_lossy(cursor)
}

struct LossyState<A: Allocator> {
    /// Field names asked for, in order asked.
    asked: Vec<&'static str, A>,
    /// Preserved unknown sub-maps merged up from nested lossy struct reads.
    nested_unknown: Map<A>,
    /// Dotted paths of defaulted fields (both at this level and from nested structs).
    defaulted: Vec<String<A>, A>,
}

/// A map being read as a struct inside [`Cursor::fields_lossy`]: required fields are strict,
/// defaulted fields record their exact dotted path when missing or null, nested lossy structs
/// merge their `unknown` and `defaulted` with full paths, and every unconsumed key is captured
/// into [`Decoded::unknown`].
pub struct LossyFields<'f, 'v, 'p, A: Allocator + Clone> {
    cursor: &'f Cursor<'v, 'p, A>,
    map: &'v Map<A>,
    state: RefCell<LossyState<A>>,
}

impl<'f, 'v, 'p, A: Allocator + Clone> LossyFields<'f, 'v, 'p, A> {
    fn new(cursor: &'f Cursor<'v, 'p, A>) -> Result<LossyFields<'f, 'v, 'p, A>, ShapeError<A>> {
        let map = cursor.map()?;
        let alloc = cursor.alloc().clone();
        Ok(LossyFields {
            cursor,
            map,
            state: RefCell::new(LossyState {
                asked: Vec::new_in(alloc.clone()),
                nested_unknown: Map::new_in(alloc.clone()),
                defaulted: Vec::new_in(alloc),
            }),
        })
    }

    fn ask(&self, key: &'static str) {
        let mut state = self.state.borrow_mut();
        if !state.asked.iter().any(|name| *name == key) {
            state.asked.push(key);
        }
    }

    fn record_defaulted(&self, key: &str) {
        let path = self.cursor.at().key(key).render_in(self.cursor.alloc().clone());
        self.state.borrow_mut().defaulted.push(path);
    }

    /// Merges a child [`Decoded`]'s `unknown` and `defaulted` into this struct's accumulator
    /// under `key`, returning the child's value.
    pub fn merge_child<T>(&self, key: &'static str, child: Decoded<T, A>) -> T {
        self.ask(key);
        let alloc = self.cursor.alloc().clone();
        let mut state = self.state.borrow_mut();
        if !child.unknown.is_empty() {
            if let Some(Value::Map(existing)) = state.nested_unknown.get_mut(key) {
                merge_unknown_into(existing, &child.unknown, alloc.clone());
            } else {
                state.nested_unknown.insert(
                    String::from_str_in(key, alloc),
                    Value::Map(child.unknown),
                );
            }
        }
        for path in child.defaulted {
            state.defaulted.push(path);
        }
        child.value
    }

    /// The field `key`, which must be present; when absent and a near-miss key exists, the
    /// [`Shape::Missing`](crate::Shape::Missing) note names it.
    pub fn cursor(&self, key: &'static str) -> Result<Cursor<'v, '_, A>, ShapeError<A>> {
        self.ask(key);
        self.cursor.field(key).map_err(|mut error| {
            let near = self
                .map
                .iter()
                .map(|(k, _)| k)
                .find(|k| closest(k, core::iter::once(key)).is_some());
            if let Some(near) = near {
                if write!(error.note, "found unknown field {near:?}").is_err() {
                    unreachable!("writing into a `String` does not fail");
                }
            }
            error
        })
    }

    /// The field `key`, if present.
    pub fn cursor_opt(&self, key: &'static str) -> Result<Option<Cursor<'v, '_, A>>, ShapeError<A>> {
        self.ask(key);
        self.cursor.field_opt(key)
    }

    /// A required field read strictly as `T`; missing is refused with
    /// [`Shape::Missing`](crate::Shape::Missing).
    pub fn req<T: FromValue<A>>(&self, key: &'static str) -> Result<T, ShapeError<A>> {
        self.cursor(key)?.get()
    }

    /// An optional field read as `Option<T>`; returns `Ok(None)` when missing or null without
    /// recording a default.
    pub fn opt<T: FromValue<A>>(&self, key: &'static str) -> Result<Option<T>, ShapeError<A>> {
        match self.cursor_opt(key)? {
            Some(field) if !field.is_null() => field.get().map(Some),
            _ => Ok(None),
        }
    }

    /// A defaulted field read as `T`; when missing or null, records its exact dotted path into
    /// [`Decoded::defaulted`] and computes the value from `default_fn`. Wrong value kinds are
    /// always refused (never silently replaced with the default).
    pub fn opt_or_default<T: FromValue<A>>(
        &self,
        key: &'static str,
        default_fn: impl FnOnce() -> T,
    ) -> Result<T, ShapeError<A>> {
        match self.cursor_opt(key)? {
            Some(field) if !field.is_null() => field.get(),
            _ => {
                self.record_defaulted(key);
                Ok(default_fn())
            }
        }
    }

    /// A required nested lossy struct read via [`FromValueLossy`]; merges the child's `unknown`
    /// sub-map under `key` and its `defaulted` paths (already rooted at the document root).
    pub fn req_lossy<T: FromValueLossy<A>>(&self, key: &'static str) -> Result<T, ShapeError<A>> {
        let child = {
            let field = self.cursor(key)?;
            T::from_value_lossy(&field)?
        };
        Ok(self.merge_child(key, child))
    }

    /// An optional nested lossy struct read via [`FromValueLossy`]; returns `Ok(None)` when
    /// missing or null, otherwise merges the child's `unknown` and `defaulted`.
    pub fn opt_lossy<T: FromValueLossy<A>>(
        &self,
        key: &'static str,
    ) -> Result<Option<T>, ShapeError<A>> {
        let child = match self.cursor_opt(key)? {
            Some(field) if !field.is_null() => Some(T::from_value_lossy(&field)?),
            _ => None,
        };
        Ok(child.map(|decoded| self.merge_child(key, decoded)))
    }

    /// A defaulted nested lossy struct read via [`FromValueLossy`]; when missing or null, records
    /// `key`'s exact dotted path into [`Decoded::defaulted`] and returns `default_fn()`,
    /// otherwise decodes lossily and merges the child's `unknown` and `defaulted`.
    pub fn opt_lossy_or_default<T: FromValueLossy<A>>(
        &self,
        key: &'static str,
        default_fn: impl FnOnce() -> T,
    ) -> Result<T, ShapeError<A>> {
        let child = match self.cursor_opt(key)? {
            Some(field) if !field.is_null() => Some(T::from_value_lossy(&field)?),
            _ => None,
        };
        match child {
            Some(decoded) => Ok(self.merge_child(key, decoded)),
            None => {
                self.record_defaulted(key);
                Ok(default_fn())
            }
        }
    }

    /// Marks `key` as consumed and validates its text value strictly against `allowed`, returning
    /// [`Shape::UnknownVariant`](crate::Shape::UnknownVariant) with closest-match hints on
    /// unknown versions.
    pub fn version_str(&self, key: &'static str, allowed: &[&str]) -> Result<&'v str, ShapeError<A>> {
        let field = self.cursor(key)?;
        let _ = field.one_of(allowed)?;
        field.text()
    }

    fn finish<T>(self, value: T, plant: impl Planted<LossyPlant>) -> Decoded<T, A> {
        let alloc = self.cursor.alloc().clone();
        let mut state = self.state.into_inner();
        let mut unknown = Map::new_in(alloc.clone());

        for (key, val) in self.map.iter() {
            if !state.asked.iter().any(|name| *name == key) {
                if !plant.has(LossyPlant::DropUnknownFields) {
                    unknown.insert(
                        String::from_str_in(key, alloc.clone()),
                        val.copy_in(alloc.clone()),
                    );
                }
            } else if let Some(child_unknown) = state.nested_unknown.remove(key) {
                unknown.insert(String::from_str_in(key, alloc.clone()), child_unknown);
            }
        }
        // Preserve any nested unknown entry merged under a key not directly in `self.map`.
        for (key, val) in state.nested_unknown.iter() {
            if !unknown.contains_key(key) {
                unknown.insert(
                    String::from_str_in(key, alloc.clone()),
                    val.copy_in(alloc.clone()),
                );
            }
        }

        if plant.has(LossyPlant::DropDefaultedPath) {
            let _ = state.defaulted.pop();
        }

        Decoded { value, unknown, defaulted: state.defaulted }
    }
}

impl<'v, 'p, A: Allocator + Clone> Cursor<'v, 'p, A> {
    /// This value read lossily as a `T`.
    pub fn get_lossy<T: FromValueLossy<A>>(&self) -> Result<Decoded<T, A>, ShapeError<A>> {
        T::from_value_lossy(self)
    }

    /// This map read as a struct, lossily: `read` takes each known field from [`LossyFields`],
    /// and every unconsumed key plus every defaulted field path is explicitly returned in
    /// [`Decoded`].
    pub fn fields_lossy<T, E: From<ShapeError<A>>>(
        &self,
        read: impl FnOnce(&LossyFields<'_, 'v, 'p, A>) -> Result<T, E>,
    ) -> Result<Decoded<T, A>, E> {
        self.fields_lossy_planted(NoPlant, read)
    }

    /// [`Cursor::fields_lossy`] with a control's plant. Shipped code calls
    /// [`Cursor::fields_lossy`].
    pub fn fields_lossy_planted<T, E: From<ShapeError<A>>>(
        &self,
        plant: impl Planted<LossyPlant>,
        read: impl FnOnce(&LossyFields<'_, 'v, 'p, A>) -> Result<T, E>,
    ) -> Result<Decoded<T, A>, E> {
        let fields = LossyFields::new(self)?;
        let value = read(&fields)?;
        Ok(fields.finish(value, plant))
    }
}

/// Merges preserved unknown fields from [`Decoded::unknown`] back into a re-serialized `map`
/// without overwriting any field currently present in `map`.
///
/// When both `map[k]` and `unknown[k]` are [`Value::Map`], their entries are merged iteratively
/// (using an explicit stack in `alloc`, never recursing on depth) so nested unknown fields from
/// newer schema versions survive a downgrade + edit + re-upgrade cycle with zero data loss.
pub fn merge_unknown_into<A: Allocator + Clone>(map: &mut Map<A>, unknown: &Map<A>, alloc: A) {
    merge_unknown_into_planted(map, unknown, alloc, NoPlant);
}

/// [`merge_unknown_into`] with a control's plant. Shipped code calls [`merge_unknown_into`].
pub fn merge_unknown_into_planted<A: Allocator + Clone>(
    map: &mut Map<A>,
    unknown: &Map<A>,
    alloc: A,
    plant: impl Planted<LossyPlant>,
) {
    if unknown.is_empty() {
        return;
    }
    let mut stack: Vec<(&mut Map<A>, &Map<A>), A> = Vec::new_in(alloc.clone());
    stack.push((map, unknown));
    while let Some((dst, src)) = stack.pop() {
        let existing = dst.len();
        for (key, src_val) in src.iter() {
            if !dst.contains_key(key) {
                dst.insert(
                    String::from_str_in(key, alloc.clone()),
                    src_val.copy_in(alloc.clone()),
                );
            } else if plant.has(LossyPlant::OverwriteOnMerge)
                && !matches!((dst.get(key), src_val), (Some(Value::Map(_)), Value::Map(_)))
            {
                dst.insert(
                    String::from_str_in(key, alloc.clone()),
                    src_val.copy_in(alloc.clone()),
                );
            }
        }
        // Only descend into entries that already existed in `dst` before the inserts above.
        for (key, dst_val) in Map::values_mut(dst).take(existing) {
            if let Value::Map(dst_child) = dst_val {
                if let Some(Value::Map(src_child)) = src.get(key) {
                    if !src_child.is_empty() {
                        stack.push((dst_child, src_child));
                    }
                }
            }
        }
    }
}

/// Reads and strictly validates a finite string schema/version tag at `key` against `allowed`.
///
/// Refuses a missing key with [`Shape::Missing`](crate::Shape::Missing), a non-text value with
/// [`Shape::Expected`](crate::Shape::Expected), and any tag outside `allowed` with
/// [`Shape::UnknownVariant`](crate::Shape::UnknownVariant) naming the closest match and all
/// allowed versions.
pub fn decode_versioned_str<'v, A: Allocator + Clone>(
    cursor: &Cursor<'v, '_, A>,
    key: &str,
    allowed: &[&str],
) -> Result<&'v str, ShapeError<A>> {
    let map = cursor.map()?;
    let field = cursor.field(key).map_err(|mut error| {
        let near = map
            .iter()
            .map(|(k, _)| k)
            .find(|k| closest(k, core::iter::once(key)).is_some());
        if let Some(near) = near {
            if write!(error.note, "found unknown field {near:?}").is_err() {
                unreachable!("writing into a `String` does not fail");
            }
        }
        error
    })?;
    let _ = field.one_of(allowed)?;
    field.text()
}

/// Per-item result inside an [`IsolatedList`].
pub enum ItemOutcome<T, A: Allocator> {
    /// The item decoded cleanly.
    Ok(T),
    /// The item failed to decode; its index, exact path [`ShapeError`], and raw [`Value`] are
    /// preserved so callers can report, quarantine, or re-emit it without discarding healthy
    /// sibling items.
    Degraded {
        index: usize,
        error: ShapeError<A>,
        raw: Value<A>,
    },
}

impl<T, A: Allocator> ItemOutcome<T, A> {
    pub fn is_ok(&self) -> bool {
        matches!(self, ItemOutcome::Ok(_))
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, ItemOutcome::Degraded { .. })
    }

    pub fn ok(&self) -> Option<&T> {
        match self {
            ItemOutcome::Ok(value) => Some(value),
            ItemOutcome::Degraded { .. } => None,
        }
    }

    pub fn into_ok(self) -> Option<T> {
        match self {
            ItemOutcome::Ok(value) => Some(value),
            ItemOutcome::Degraded { .. } => None,
        }
    }

    pub fn error(&self) -> Option<&ShapeError<A>> {
        match self {
            ItemOutcome::Ok(_) => None,
            ItemOutcome::Degraded { error, .. } => Some(error),
        }
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for ItemOutcome<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemOutcome::Ok(value) => f.debug_tuple("Ok").field(value).finish(),
            ItemOutcome::Degraded { index, error, raw } => f
                .debug_struct("Degraded")
                .field("index", index)
                .field("error", error)
                .field("raw", raw)
                .finish(),
        }
    }
}

/// A list decoded with per-item fault isolation via [`decode_list_isolated`].
pub struct IsolatedList<T, A: Allocator> {
    pub items: Vec<ItemOutcome<T, A>, A>,
    pub degraded_count: usize,
}

impl<T, A: Allocator> IsolatedList<T, A> {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn ok_count(&self) -> usize {
        self.items.len() - self.degraded_count
    }

    pub fn degraded_count(&self) -> usize {
        self.degraded_count
    }

    pub fn is_all_ok(&self) -> bool {
        self.degraded_count == 0
    }

    pub fn has_degraded(&self) -> bool {
        self.degraded_count > 0
    }

    /// Iterates over every cleanly decoded item in order.
    pub fn ok_items(&self) -> impl Iterator<Item = &T> + '_ {
        self.items.iter().filter_map(ItemOutcome::ok)
    }

    /// Iterates over every degraded item's `(index, error, raw)` in order.
    pub fn degraded_items(&self) -> impl Iterator<Item = (usize, &ShapeError<A>, &Value<A>)> + '_ {
        self.items.iter().filter_map(|item| match item {
            ItemOutcome::Ok(_) => None,
            ItemOutcome::Degraded { index, error, raw } => Some((*index, error, raw)),
        })
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for IsolatedList<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsolatedList")
            .field("items", &self.items)
            .field("degraded_count", &self.degraded_count)
            .finish()
    }
}

/// Decodes every element of a list independently so one corrupt or incompatible item becomes
/// [`ItemOutcome::Degraded`] (preserving its index, path [`ShapeError`], and raw [`Value`])
/// instead of discarding the entire list.
///
/// Refuses a non-list container itself with [`Shape::Expected`](crate::Shape::Expected).
pub fn decode_list_isolated<'v, T, A, F>(
    cursor: &Cursor<'v, '_, A>,
    mut decode_one: F,
) -> Result<IsolatedList<T, A>, ShapeError<A>>
where
    A: Allocator + Clone,
    F: FnMut(&Cursor<'v, '_, A>) -> Result<T, ShapeError<A>>,
{
    let list = cursor.list()?;
    let alloc = cursor.alloc().clone();
    let mut items = Vec::with_capacity_in(list.len(), alloc.clone());
    let mut degraded_count = 0usize;
    let mut index = 0usize;
    cursor.each(|item_cursor| {
        match decode_one(&item_cursor) {
            Ok(decoded) => items.push(ItemOutcome::Ok(decoded)),
            Err(error) => {
                degraded_count += 1;
                items.push(ItemOutcome::Degraded {
                    index,
                    error,
                    raw: item_cursor.value().copy_in(alloc.clone()),
                });
            }
        }
        index += 1;
        Ok::<(), ShapeError<A>>(())
    })?;
    Ok(IsolatedList { items, degraded_count })
}
