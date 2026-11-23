//! Pure `#![no_std]` structured query and patching over [`blaze_data::Value`].
//!
//! Works uniformly across JSON, YAML, TOML, CBOR, and TAB documents loaded into
//! [`blaze_data::Value`]:
//!
//! - **RFC 6901 JSON Pointer**: [`get`], [`get_mut`], [`set`], [`remove`], and
//!   [`escape_token_into`] with exact `~1` -> `/`, `~0` -> `~` decoding order and strict
//!   non-leading-zero array indices.
//! - **RFC 7396 JSON Merge Patch**: [`merge_patch`] (100% iterative with an explicit stack
//!   and caller-chosen `max_depth`).
//! - **Bounded JSONPath subset**: [`query`] with [`PathLimits`] supporting `$`, `.field`,
//!   `['quoted']`, `[index]` (including negative indices from the end), `[*]` / `.*`,
//!   `[start:end]` / `[start:end:step]` slices, and iterative `..field` / `..*` recursive
//!   descent.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::cmp::Ordering;
use core::fmt;

use blaze_data::{Map, Value};
use blaze_own::{AllocFailure, String, Vec};

const REMOVE_ESCAPED_STACK_CAP: usize = 4096;

/// Bounds enforced when evaluating JSONPath queries against untrusted expressions or values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PathLimits {
    /// Maximum number of parsed path segments in a JSONPath expression.
    pub most_segments: usize,
    /// Maximum number of matched values returned by [`query`].
    pub most_matches: usize,
    /// Maximum traversal steps executed during [`query`].
    pub most_steps: usize,
}

impl PathLimits {
    /// Generous default bounds (128 segments, 16,384 matches, 250,000 steps).
    pub const USUAL: Self = Self {
        most_segments: 128,
        most_matches: 16_384,
        most_steps: 250_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed failure returned by JSON Pointer, JSON Merge Patch, or JSONPath operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerError {
    /// A non-empty JSON Pointer did not begin with `/`.
    MissingLeadingSlash,
    /// A JSON Pointer contained an invalid `~` escape (anything other than `~0` or `~1`).
    InvalidEscape { offset: usize },
    /// A reference token targeting a list was not a valid RFC 6901 array index (or `-`).
    InvalidArrayIndex,
    /// An array index was out of bounds for the target list.
    IndexOutOfBounds { index: usize, len: usize },
    /// Attempted to index or mutate a non-container value.
    NotContainer,
    /// An intermediate parent along a JSON Pointer did not exist during [`set`].
    MissingParent,
    /// Attempted to remove the root document (`""`) via [`remove`].
    CannotRemoveRoot,
    /// An escaped map key passed to [`remove`] exceeded the stack decode buffer.
    TokenTooLong { limit: usize },
    /// RFC 7396 merge patch nesting exceeded `max_depth`.
    MaxDepthExceeded { limit: usize },
    /// JSONPath expression did not start with `$` or contained invalid syntax.
    InvalidPathSyntax { offset: usize },
    /// JSONPath expression exceeded [`PathLimits::most_segments`].
    TooManySegments { limit: usize },
    /// JSONPath evaluation exceeded [`PathLimits::most_matches`].
    TooManyMatches { limit: usize },
    /// JSONPath evaluation exceeded [`PathLimits::most_steps`].
    StepLimitExceeded { limit: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for PointerError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for PointerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PointerError::MissingLeadingSlash => {
                f.write_str("bstd::pointer: non-empty JSON Pointer must start with `/`")
            }
            PointerError::InvalidEscape { offset } => {
                write!(f, "bstd::pointer: invalid `~` escape sequence at byte {offset}")
            }
            PointerError::InvalidArrayIndex => {
                f.write_str("bstd::pointer: invalid RFC 6901 array index")
            }
            PointerError::IndexOutOfBounds { index, len } => {
                write!(f, "bstd::pointer: array index {index} out of bounds for length {len}")
            }
            PointerError::NotContainer => {
                f.write_str("bstd::pointer: target value is not a map or list container")
            }
            PointerError::MissingParent => {
                f.write_str("bstd::pointer: intermediate parent container does not exist")
            }
            PointerError::CannotRemoveRoot => {
                f.write_str("bstd::pointer: cannot remove the root document (`\"\"`)")
            }
            PointerError::TokenTooLong { limit } => {
                write!(f, "bstd::pointer: escaped map key exceeds {limit}-byte decode limit")
            }
            PointerError::MaxDepthExceeded { limit } => {
                write!(f, "bstd::pointer: merge patch nesting depth exceeds limit {limit}")
            }
            PointerError::InvalidPathSyntax { offset } => {
                write!(f, "bstd::pointer: invalid JSONPath syntax at byte {offset}")
            }
            PointerError::TooManySegments { limit } => {
                write!(f, "bstd::pointer: JSONPath segment count exceeds limit {limit}")
            }
            PointerError::TooManyMatches { limit } => {
                write!(f, "bstd::pointer: JSONPath match count exceeds limit {limit}")
            }
            PointerError::StepLimitExceeded { limit } => {
                write!(f, "bstd::pointer: JSONPath evaluation exceeded step limit of {limit}")
            }
            PointerError::Alloc(err) => write!(f, "bstd::pointer: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for PointerError {}

// ------------------------------------------------------------------------------------------------
// RFC 6901 JSON Pointer
// ------------------------------------------------------------------------------------------------

/// Escape a single RFC 6901 reference token (`~` -> `~0`, `/` -> `~1`) into `out`.
pub fn escape_token_into<W: fmt::Write>(token: &str, out: &mut W) -> fmt::Result {
    let bytes = token.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'~' || b == b'/' {
            if i > start
                && let Some(chunk) = token.get(start..i)
            {
                out.write_str(chunk)?;
            }
            if b == b'~' {
                out.write_str("~0")?;
            } else {
                out.write_str("~1")?;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < token.len()
        && let Some(tail) = token.get(start..)
    {
        out.write_str(tail)?;
    }
    Ok(())
}

/// Resolve an RFC 6901 JSON Pointer against `root`.
///
/// - `""` returns `Ok(Some(root))`.
/// - Non-empty pointers must start with `/`; reference tokens decode `~1` -> `/` and `~0` -> `~`
///   in exact RFC 6901 order.
/// - Array indices strictly forbid leading zeros (`"0"` is valid; `"01"` returns `Ok(None)`).
pub fn get<'a, A: Allocator>(
    root: &'a Value<A>,
    pointer: &str,
) -> Result<Option<&'a Value<A>>, PointerError> {
    get_planted(root, pointer, false)
}

fn get_planted<'a, A: Allocator>(
    root: &'a Value<A>,
    pointer: &str,
    plant_wrong_unescape_order: bool,
) -> Result<Option<&'a Value<A>>, PointerError> {
    validate_pointer_syntax(pointer)?;
    if pointer.is_empty() {
        return Ok(Some(root));
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(PointerError::MissingLeadingSlash);
    };

    let mut cur = root;
    for token in rest.split('/') {
        cur = match cur {
            Value::Map(map) => {
                if !token.contains('~') {
                    match map.get(token) {
                        Some(next) => next,
                        None => return Ok(None),
                    }
                } else {
                    match find_in_map_unescaped(map, token, plant_wrong_unescape_order) {
                        Some(next) => next,
                        None => return Ok(None),
                    }
                }
            }
            Value::List(list) => match parse_array_index(token) {
                Some(idx) => match list.get(idx) {
                    Some(next) => next,
                    None => return Ok(None),
                },
                None => return Ok(None),
            },
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_) => return Ok(None),
        };
    }
    Ok(Some(cur))
}

/// Resolve an RFC 6901 JSON Pointer mutably against `root`.
pub fn get_mut<'a, A: Allocator>(
    root: &'a mut Value<A>,
    pointer: &str,
) -> Result<Option<&'a mut Value<A>>, PointerError> {
    get_mut_planted(root, pointer, false)
}

fn get_mut_planted<'a, A: Allocator>(
    root: &'a mut Value<A>,
    pointer: &str,
    plant_wrong_unescape_order: bool,
) -> Result<Option<&'a mut Value<A>>, PointerError> {
    validate_pointer_syntax(pointer)?;
    if pointer.is_empty() {
        return Ok(Some(root));
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(PointerError::MissingLeadingSlash);
    };

    let mut cur = root;
    for token in rest.split('/') {
        cur = match cur {
            Value::Map(map) => {
                if !token.contains('~') {
                    match map.get_mut(token) {
                        Some(next) => next,
                        None => return Ok(None),
                    }
                } else {
                    let mut found = None;
                    for (k, v) in map.values_mut() {
                        if eq_unescaped(k, token, plant_wrong_unescape_order) {
                            found = Some(v);
                            break;
                        }
                    }
                    match found {
                        Some(next) => next,
                        None => return Ok(None),
                    }
                }
            }
            Value::List(list) => match parse_array_index(token) {
                Some(idx) => match list.get_mut(idx) {
                    Some(next) => next,
                    None => return Ok(None),
                },
                None => return Ok(None),
            },
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_) => return Ok(None),
        };
    }
    Ok(Some(cur))
}

/// Set or insert `value` at RFC 6901 JSON Pointer `pointer` inside `root`.
///
/// - `""` replaces `*root` with `value`.
/// - On a [`Value::Map`], replaces an existing key or inserts a new one.
/// - On a [`Value::List`], `"-"` appends to the end and an in-bounds index replaces the slot.
pub fn set<A: Allocator + Copy>(
    root: &mut Value<A>,
    pointer: &str,
    value: Value<A>,
    alloc: A,
) -> Result<(), PointerError> {
    set_planted(root, pointer, value, alloc, false)
}

fn set_planted<A: Allocator + Copy>(
    root: &mut Value<A>,
    pointer: &str,
    value: Value<A>,
    alloc: A,
    plant_wrong_unescape_order: bool,
) -> Result<(), PointerError> {
    validate_pointer_syntax(pointer)?;
    if pointer.is_empty() {
        *root = value;
        return Ok(());
    }

    let Some(last_slash) = pointer.rfind('/') else {
        return Err(PointerError::MissingLeadingSlash);
    };
    let parent_ptr = match pointer.get(..last_slash) {
        Some(p) => p,
        None => return Err(PointerError::MissingLeadingSlash),
    };
    let last_token = match pointer.get(last_slash + 1..) {
        Some(t) => t,
        None => return Err(PointerError::MissingLeadingSlash),
    };

    let Some(parent) = get_mut_planted(root, parent_ptr, plant_wrong_unescape_order)? else {
        return Err(PointerError::MissingParent);
    };

    match parent {
        Value::Map(map) => {
            let key = unescape_token_to_string(last_token, alloc, plant_wrong_unescape_order)?;
            let _ = map.insert(key, value);
            Ok(())
        }
        Value::List(list) => {
            if last_token == "-" {
                list.try_push(value)
                    .map_err(|r| PointerError::Alloc(r.why))?;
                return Ok(());
            }
            let Some(idx) = parse_array_index(last_token) else {
                return Err(PointerError::InvalidArrayIndex);
            };
            let len = list.len();
            let Some(slot) = list.get_mut(idx) else {
                return Err(PointerError::IndexOutOfBounds { index: idx, len });
            };
            *slot = value;
            Ok(())
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_) => Err(PointerError::NotContainer),
    }
}

/// Remove and return the value at RFC 6901 JSON Pointer `pointer` inside `root`.
pub fn remove<A: Allocator + Copy>(
    root: &mut Value<A>,
    pointer: &str,
) -> Result<Option<Value<A>>, PointerError> {
    remove_planted(root, pointer, false)
}

fn remove_planted<A: Allocator + Copy>(
    root: &mut Value<A>,
    pointer: &str,
    plant_wrong_unescape_order: bool,
) -> Result<Option<Value<A>>, PointerError> {
    validate_pointer_syntax(pointer)?;
    if pointer.is_empty() {
        return Err(PointerError::CannotRemoveRoot);
    }

    let Some(last_slash) = pointer.rfind('/') else {
        return Err(PointerError::MissingLeadingSlash);
    };
    let parent_ptr = match pointer.get(..last_slash) {
        Some(p) => p,
        None => return Err(PointerError::MissingLeadingSlash),
    };
    let last_token = match pointer.get(last_slash + 1..) {
        Some(t) => t,
        None => return Err(PointerError::MissingLeadingSlash),
    };

    let Some(parent) = get_mut_planted(root, parent_ptr, plant_wrong_unescape_order)? else {
        return Ok(None);
    };

    match parent {
        Value::Map(map) => {
            if !last_token.contains('~') {
                Ok(map.remove(last_token))
            } else {
                remove_escaped_map_key(map, last_token, plant_wrong_unescape_order)
            }
        }
        Value::List(list) => {
            let Some(idx) = parse_array_index(last_token) else {
                return Ok(None);
            };
            if idx >= list.len() {
                return Ok(None);
            }
            Ok(Some(list.remove_shift(idx)))
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_) => Ok(None),
    }
}

fn remove_escaped_map_key<A: Allocator>(
    map: &mut Map<A>,
    raw_token: &str,
    plant_wrong_unescape_order: bool,
) -> Result<Option<Value<A>>, PointerError> {
    let mut buf = [0u8; REMOVE_ESCAPED_STACK_CAP];
    let mut len = 0usize;
    let mut iter = UnescapedBytes::new(raw_token, plant_wrong_unescape_order);
    while let Some(b) = iter.next_byte() {
        let Some(slot) = buf.get_mut(len) else {
            return Err(PointerError::TokenTooLong { limit: REMOVE_ESCAPED_STACK_CAP });
        };
        *slot = b;
        len += 1;
    }
    let Some(slice) = buf.get(..len) else {
        return Err(PointerError::TokenTooLong { limit: REMOVE_ESCAPED_STACK_CAP });
    };
    let Ok(decoded) = core::str::from_utf8(slice) else {
        return Err(PointerError::InvalidEscape { offset: 0 });
    };
    Ok(map.remove(decoded))
}

fn validate_pointer_syntax(pointer: &str) -> Result<(), PointerError> {
    if pointer.is_empty() {
        return Ok(());
    }
    let bytes = pointer.as_bytes();
    if bytes.first() != Some(&b'/') {
        return Err(PointerError::MissingLeadingSlash);
    }
    let mut i = 1usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'~' {
            match bytes.get(i + 1) {
                Some(b'0' | b'1') => {
                    i += 2;
                    continue;
                }
                _ => return Err(PointerError::InvalidEscape { offset: i }),
            }
        }
        i += 1;
    }
    Ok(())
}

fn parse_array_index(token: &str) -> Option<usize> {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes.len() == 1 {
        let &b = bytes.first()?;
        if b.is_ascii_digit() {
            return Some(usize::from(b - b'0'));
        }
        return None;
    }
    // Multi-digit array indices must not start with '0' (RFC 6901 §4).
    let &first = bytes.first()?;
    if !matches!(first, b'1'..=b'9') {
        return None;
    }
    let mut acc = 0usize;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(acc)
}

struct UnescapedBytes<'a> {
    bytes: &'a [u8],
    pos: usize,
    plant_wrong_order: bool,
}

impl<'a> UnescapedBytes<'a> {
    fn new(raw: &'a str, plant_wrong_order: bool) -> Self {
        Self { bytes: raw.as_bytes(), pos: 0, plant_wrong_order }
    }

    fn next_byte(&mut self) -> Option<u8> {
        let &b = self.bytes.get(self.pos)?;
        if b == b'~' {
            if self.plant_wrong_order
                && self.bytes.get(self.pos + 1) == Some(&b'0')
                && self.bytes.get(self.pos + 2) == Some(&b'1')
            {
                self.pos += 3;
                return Some(b'/');
            }
            if let Some(&next) = self.bytes.get(self.pos + 1) {
                if next == b'0' {
                    self.pos += 2;
                    return Some(b'~');
                }
                if next == b'1' {
                    self.pos += 2;
                    return Some(b'/');
                }
            }
        }
        self.pos += 1;
        Some(b)
    }
}

fn cmp_unescaped(key: &str, raw_token: &str, plant_wrong_order: bool) -> Ordering {
    let mut kb = key.as_bytes().iter().copied();
    let mut tb = UnescapedBytes::new(raw_token, plant_wrong_order);
    loop {
        match (kb.next(), tb.next_byte()) {
            (Some(a), Some(b)) => {
                let ord = a.cmp(&b);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

fn eq_unescaped(key: &str, raw_token: &str, plant_wrong_order: bool) -> bool {
    cmp_unescaped(key, raw_token, plant_wrong_order) == Ordering::Equal
}

fn find_in_map_unescaped<'a, A: Allocator>(
    map: &'a Map<A>,
    raw_token: &str,
    plant_wrong_order: bool,
) -> Option<&'a Value<A>> {
    let mut lo = 0usize;
    let mut hi = map.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (k, v) = map.sorted_at(mid)?;
        match cmp_unescaped(k, raw_token, plant_wrong_order) {
            Ordering::Equal => return Some(v),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    None
}

fn unescape_token_to_string<A: Allocator + Copy>(
    raw_token: &str,
    alloc: A,
    plant_wrong_order: bool,
) -> Result<String<A>, PointerError> {
    if !raw_token.contains('~') {
        return Ok(String::try_from_str_in(raw_token, alloc)?);
    }
    let mut bytes = Vec::try_with_capacity_in(raw_token.len(), alloc)?;
    let mut iter = UnescapedBytes::new(raw_token, plant_wrong_order);
    while let Some(b) = iter.next_byte() {
        bytes
            .try_push(b)
            .map_err(|r| PointerError::Alloc(r.why))?;
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(_) => Err(PointerError::InvalidEscape { offset: 0 }),
    }
}

// ------------------------------------------------------------------------------------------------
// RFC 7396 JSON Merge Patch (100% iterative, bounded depth)
// ------------------------------------------------------------------------------------------------

struct MergeFrame<A: Allocator> {
    parent_key: Option<String<A>>,
    target_map: Map<A>,
    pending: Vec<(String<A>, Value<A>), A>,
}

fn drain_map_entries<A: Allocator + Copy>(
    map: &mut Map<A>,
    alloc: A,
) -> Result<Vec<(String<A>, Value<A>), A>, PointerError> {
    let mut out = Vec::try_with_capacity_in(map.len(), alloc)?;
    for (k, v) in map.values_mut() {
        let key = String::try_from_str_in(k, alloc)?;
        let val = core::mem::replace(v, Value::Null);
        out.try_push((key, val))
            .map_err(|r| PointerError::Alloc(r.why))?;
    }
    out.as_mut_slice().reverse();
    Ok(out)
}

/// Apply an RFC 7396 JSON Merge Patch to `target`, iteratively up to `max_depth` nested objects.
///
/// - Non-map patches replace `target` wholesale (including `Value::Null` and `Value::List`).
/// - Inside map patches, `Value::Null` deletes the corresponding key from the target map; nested
///   maps merge recursively via an explicit stack; arrays and other scalars replace wholesale.
pub fn merge_patch<A: Allocator + Copy>(
    target: Value<A>,
    patch: Value<A>,
    max_depth: usize,
    alloc: A,
) -> Result<Value<A>, PointerError> {
    match &patch {
        Value::Map(_) => {}
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::List(_) => return Ok(patch),
    }

    if max_depth == 0 {
        return Err(PointerError::MaxDepthExceeded { limit: max_depth });
    }

    let mut root_patch_map = match patch.into_map() {
        Some(m) => m,
        None => return Ok(Value::Null),
    };
    let root_target_map = match target.into_map() {
        Some(m) => m,
        None => Map::new_in(alloc),
    };
    let root_pending = drain_map_entries(&mut root_patch_map, alloc)?;

    let mut stack: Vec<MergeFrame<A>, A> = Vec::new_in(alloc);
    stack
        .try_push(MergeFrame {
            parent_key: None,
            target_map: root_target_map,
            pending: root_pending,
        })
        .map_err(|r| PointerError::Alloc(r.why))?;

    loop {
        let depth = stack.len();
        let Some(top) = stack.last_mut() else {
            break;
        };
        if let Some((key, patch_val)) = top.pending.pop() {
            match patch_val {
                Value::Null => {
                    let _ = top.target_map.remove(key.as_str());
                }
                Value::Map(_) => {
                    if depth >= max_depth {
                        return Err(PointerError::MaxDepthExceeded { limit: max_depth });
                    }
                    let existing_child = top.target_map.remove(key.as_str());
                    let child_target_map = match existing_child {
                        Some(v) => match v.into_map() {
                            Some(m) => m,
                            None => Map::new_in(alloc),
                        },
                        None => Map::new_in(alloc),
                    };
                    let mut child_patch_map = match patch_val.into_map() {
                        Some(m) => m,
                        None => Map::new_in(alloc),
                    };
                    let child_pending = drain_map_entries(&mut child_patch_map, alloc)?;
                    stack
                        .try_push(MergeFrame {
                            parent_key: Some(key),
                            target_map: child_target_map,
                            pending: child_pending,
                        })
                        .map_err(|r| PointerError::Alloc(r.why))?;
                }
                Value::Bool(_)
                | Value::Int(_)
                | Value::Float(_)
                | Value::Text(_)
                | Value::Bytes(_)
                | Value::List(_) => {
                    let _ = top.target_map.insert(key, patch_val);
                }
            }
        } else {
            let Some(done) = stack.pop() else {
                break;
            };
            let finished = Value::Map(done.target_map);
            match (done.parent_key, stack.last_mut()) {
                (Some(key), Some(parent)) => {
                    let _ = parent.target_map.insert(key, finished);
                }
                (None, None) | (Some(_), None) | (None, Some(_)) => {
                    return Ok(finished);
                }
            }
        }
    }

    Ok(Value::Map(Map::new_in(alloc)))
}

// ------------------------------------------------------------------------------------------------
// Bounded Iterative JSONPath Subset
// ------------------------------------------------------------------------------------------------

enum Selector<A: Allocator> {
    Field(String<A>),
    Wildcard,
    Index(isize),
    Slice {
        start: Option<isize>,
        end: Option<isize>,
        step: isize,
    },
}

struct PathStep<A: Allocator> {
    descendant: bool,
    selector: Selector<A>,
}

/// Evaluate a JSONPath expression over `root` within `limits`, returning matched value references.
///
/// Supported syntax:
/// - `$` (root)
/// - `.field` and `['quoted']` / `["quoted"]` (map lookup)
/// - `[index]` (list index, including negative indices such as `[-1]`)
/// - `[*]` and `.*` (all children of a list or map)
/// - `[start:end]` and `[start:end:step]` (list slices, supporting negative indices)
/// - `..field`, `..*`, `..['quoted']`, `..[index]`, `..[*]`, `..[start:end]` (iterative recursive descent)
pub fn query<'a, A: Allocator + Copy>(
    root: &'a Value<A>,
    expr: &str,
    limits: PathLimits,
    alloc: A,
) -> Result<Vec<&'a Value<A>, A>, PointerError> {
    let steps = parse_jsonpath(expr, limits.most_segments, alloc)?;

    let mut current: Vec<&'a Value<A>, A> = Vec::new_in(alloc);
    if limits.most_matches == 0 {
        return Err(PointerError::TooManyMatches { limit: 0 });
    }
    current
        .try_push(root)
        .map_err(|r| PointerError::Alloc(r.why))?;

    let mut next: Vec<&'a Value<A>, A> = Vec::new_in(alloc);
    let mut worklist: Vec<&'a Value<A>, A> = Vec::new_in(alloc);
    let mut steps_used = 0usize;

    for step in steps.as_slice() {
        next.clear();
        for &node in current.as_slice() {
            if !step.descendant {
                tick_step(&mut steps_used, limits.most_steps)?;
                apply_selector(
                    node,
                    &step.selector,
                    &mut next,
                    &mut steps_used,
                    limits,
                )?;
            } else {
                worklist.clear();
                worklist
                    .try_push(node)
                    .map_err(|r| PointerError::Alloc(r.why))?;
                while let Some(v) = worklist.pop() {
                    tick_step(&mut steps_used, limits.most_steps)?;
                    apply_selector(
                        v,
                        &step.selector,
                        &mut next,
                        &mut steps_used,
                        limits,
                    )?;
                    match v {
                        Value::List(items) => {
                            let mut idx = items.len();
                            while idx > 0 {
                                idx -= 1;
                                if let Some(child) = items.get(idx) {
                                    worklist
                                        .try_push(child)
                                        .map_err(|r| PointerError::Alloc(r.why))?;
                                }
                            }
                        }
                        Value::Map(map) => {
                            let mut idx = map.len();
                            while idx > 0 {
                                idx -= 1;
                                if let Some((_, child)) = map.entry_at(idx) {
                                    worklist
                                        .try_push(child)
                                        .map_err(|r| PointerError::Alloc(r.why))?;
                                }
                            }
                        }
                        Value::Null
                        | Value::Bool(_)
                        | Value::Int(_)
                        | Value::Float(_)
                        | Value::Text(_)
                        | Value::Bytes(_) => {}
                    }
                }
            }
        }
        core::mem::swap(&mut current, &mut next);
        if current.is_empty() {
            break;
        }
    }

    Ok(current)
}

#[inline]
fn tick_step(steps_used: &mut usize, limit: usize) -> Result<(), PointerError> {
    if *steps_used >= limit {
        return Err(PointerError::StepLimitExceeded { limit });
    }
    *steps_used += 1;
    Ok(())
}

#[inline]
fn push_match<'a, A: Allocator>(
    out: &mut Vec<&'a Value<A>, A>,
    val: &'a Value<A>,
    most_matches: usize,
) -> Result<(), PointerError> {
    if out.len() >= most_matches {
        return Err(PointerError::TooManyMatches { limit: most_matches });
    }
    out.try_push(val)
        .map_err(|r| PointerError::Alloc(r.why))
}

fn apply_selector<'a, A: Allocator>(
    node: &'a Value<A>,
    selector: &Selector<A>,
    out: &mut Vec<&'a Value<A>, A>,
    steps_used: &mut usize,
    limits: PathLimits,
) -> Result<(), PointerError> {
    match selector {
        Selector::Field(name) => match node {
            Value::Map(map) => {
                if let Some(v) = map.get(name.as_str()) {
                    push_match(out, v, limits.most_matches)?;
                }
                Ok(())
            }
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_)
            | Value::List(_) => Ok(()),
        },
        Selector::Wildcard => match node {
            Value::List(items) => {
                for item in items.as_slice() {
                    tick_step(steps_used, limits.most_steps)?;
                    push_match(out, item, limits.most_matches)?;
                }
                Ok(())
            }
            Value::Map(map) => {
                for (_, val) in map.iter() {
                    tick_step(steps_used, limits.most_steps)?;
                    push_match(out, val, limits.most_matches)?;
                }
                Ok(())
            }
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_) => Ok(()),
        },
        Selector::Index(idx) => match node {
            Value::List(items) => {
                if let Some(target) = resolve_list_index(items.len(), *idx)
                    && let Some(item) = items.get(target)
                {
                    push_match(out, item, limits.most_matches)?;
                }
                Ok(())
            }
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_)
            | Value::Map(_) => Ok(()),
        },
        Selector::Slice { start, end, step } => match node {
            Value::List(items) => {
                apply_slice(items.as_slice(), *start, *end, *step, out, steps_used, limits)
            }
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_)
            | Value::Map(_) => Ok(()),
        },
    }
}

fn resolve_list_index(len: usize, idx: isize) -> Option<usize> {
    if idx >= 0 {
        usize::try_from(idx).ok().filter(|&u| u < len)
    } else {
        let len_i = i128::try_from(len).ok()?;
        let target = len_i + i128::try_from(idx).ok()?;
        if target < 0 {
            return None;
        }
        usize::try_from(target).ok().filter(|&u| u < len)
    }
}

fn normalize_slice_bound(bound: isize, len: i128) -> i128 {
    let b = i128::try_from(bound).unwrap_or(0);
    if b >= 0 { b } else { len + b }
}

fn apply_slice<'a, A: Allocator>(
    items: &'a [Value<A>],
    start: Option<isize>,
    end: Option<isize>,
    step: isize,
    out: &mut Vec<&'a Value<A>, A>,
    steps_used: &mut usize,
    limits: PathLimits,
) -> Result<(), PointerError> {
    if step == 0 || items.is_empty() {
        return Ok(());
    }
    let Ok(len_i) = i128::try_from(items.len()) else {
        return Ok(());
    };
    let Ok(step_i) = i128::try_from(step) else {
        return Ok(());
    };

    if step > 0 {
        let lower = match start {
            Some(s) => normalize_slice_bound(s, len_i).clamp(0, len_i),
            None => 0,
        };
        let upper = match end {
            Some(e) => normalize_slice_bound(e, len_i).clamp(0, len_i),
            None => len_i,
        };
        let mut i = lower;
        while i < upper {
            tick_step(steps_used, limits.most_steps)?;
            if let Ok(u) = usize::try_from(i)
                && let Some(item) = items.get(u)
            {
                push_match(out, item, limits.most_matches)?;
            }
            i += step_i;
        }
    } else {
        let upper = match start {
            Some(s) => normalize_slice_bound(s, len_i).clamp(-1, len_i - 1),
            None => len_i - 1,
        };
        let lower = match end {
            Some(e) => normalize_slice_bound(e, len_i).clamp(-1, len_i - 1),
            None => -1,
        };
        let mut i = upper;
        while i > lower {
            tick_step(steps_used, limits.most_steps)?;
            if let Ok(u) = usize::try_from(i)
                && let Some(item) = items.get(u)
            {
                push_match(out, item, limits.most_matches)?;
            }
            i += step_i;
        }
    }
    Ok(())
}

fn parse_jsonpath<A: Allocator + Copy>(
    expr: &str,
    most_segments: usize,
    alloc: A,
) -> Result<Vec<PathStep<A>, A>, PointerError> {
    let bytes = expr.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(PointerError::InvalidPathSyntax { offset: 0 });
    }

    let mut steps: Vec<PathStep<A>, A> = Vec::new_in(alloc);
    let mut pos = 1usize;

    while let Some(&b) = bytes.get(pos) {
        if steps.len() >= most_segments {
            return Err(PointerError::TooManySegments { limit: most_segments });
        }

        match b {
            b'.' => {
                let descendant = if bytes.get(pos + 1) == Some(&b'.') {
                    pos += 2;
                    true
                } else {
                    pos += 1;
                    false
                };

                let Some(&next) = bytes.get(pos) else {
                    return Err(PointerError::InvalidPathSyntax { offset: pos });
                };

                let selector = if next == b'*' {
                    pos += 1;
                    Selector::Wildcard
                } else if descendant && next == b'[' {
                    parse_bracket_selector(expr, &mut pos, alloc)?
                } else {
                    let field = parse_bare_field(expr, &mut pos, alloc)?;
                    Selector::Field(field)
                };

                steps
                    .try_push(PathStep { descendant, selector })
                    .map_err(|r| PointerError::Alloc(r.why))?;
            }
            b'[' => {
                let selector = parse_bracket_selector(expr, &mut pos, alloc)?;
                steps
                    .try_push(PathStep { descendant: false, selector })
                    .map_err(|r| PointerError::Alloc(r.why))?;
            }
            _ => return Err(PointerError::InvalidPathSyntax { offset: pos }),
        }
    }

    Ok(steps)
}

fn skip_ascii_spaces(bytes: &[u8], pos: &mut usize) {
    while let Some(&b) = bytes.get(*pos) {
        if b == b' ' || b == b'\t' {
            *pos += 1;
        } else {
            break;
        }
    }
}

fn parse_bare_field<A: Allocator + Copy>(
    expr: &str,
    pos: &mut usize,
    alloc: A,
) -> Result<String<A>, PointerError> {
    let start = *pos;
    let Some(tail) = expr.get(start..) else {
        return Err(PointerError::InvalidPathSyntax { offset: start });
    };

    let mut first = true;
    for ch in tail.chars() {
        let ok = if first {
            first = false;
            ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii()
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || !ch.is_ascii()
        };
        if !ok {
            break;
        }
        *pos += ch.len_utf8();
    }

    if *pos == start {
        return Err(PointerError::InvalidPathSyntax { offset: start });
    }
    let Some(name) = expr.get(start..*pos) else {
        return Err(PointerError::InvalidPathSyntax { offset: start });
    };
    Ok(String::try_from_str_in(name, alloc)?)
}

fn parse_bracket_selector<A: Allocator + Copy>(
    expr: &str,
    pos: &mut usize,
    alloc: A,
) -> Result<Selector<A>, PointerError> {
    let bytes = expr.as_bytes();
    if bytes.get(*pos) != Some(&b'[') {
        return Err(PointerError::InvalidPathSyntax { offset: *pos });
    }
    *pos += 1;
    skip_ascii_spaces(bytes, pos);

    let Some(&b) = bytes.get(*pos) else {
        return Err(PointerError::InvalidPathSyntax { offset: *pos });
    };

    match b {
        b'*' => {
            *pos += 1;
            skip_ascii_spaces(bytes, pos);
            if bytes.get(*pos) != Some(&b']') {
                return Err(PointerError::InvalidPathSyntax { offset: *pos });
            }
            *pos += 1;
            Ok(Selector::Wildcard)
        }
        b'\'' | b'"' => {
            let quote = if b == b'\'' { '\'' } else { '"' };
            *pos += 1;
            let field = parse_quoted_field(expr, pos, quote, alloc)?;
            skip_ascii_spaces(bytes, pos);
            if bytes.get(*pos) != Some(&b']') {
                return Err(PointerError::InvalidPathSyntax { offset: *pos });
            }
            *pos += 1;
            Ok(Selector::Field(field))
        }
        b':' | b'-' | b'0'..=b'9' => parse_index_or_slice(bytes, pos),
        _ => Err(PointerError::InvalidPathSyntax { offset: *pos }),
    }
}

fn parse_quoted_field<A: Allocator + Copy>(
    expr: &str,
    pos: &mut usize,
    quote: char,
    alloc: A,
) -> Result<String<A>, PointerError> {
    let start = *pos;
    let mut out = String::new_in(alloc);
    while let Some(tail) = expr.get(*pos..) {
        let Some(ch) = tail.chars().next() else {
            break;
        };
        *pos += ch.len_utf8();
        if ch == quote {
            return Ok(out);
        }
        if ch == '\\' {
            let esc_off = *pos;
            let Some(esc_tail) = expr.get(*pos..) else {
                return Err(PointerError::InvalidPathSyntax { offset: esc_off });
            };
            let Some(esc) = esc_tail.chars().next() else {
                return Err(PointerError::InvalidPathSyntax { offset: esc_off });
            };
            *pos += esc.len_utf8();
            let decoded = match esc {
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '/' => '/',
                _ => return Err(PointerError::InvalidPathSyntax { offset: esc_off }),
            };
            out.try_push(decoded)?;
        } else {
            out.try_push(ch)?;
        }
    }
    Err(PointerError::InvalidPathSyntax { offset: start })
}

fn parse_optional_isize(bytes: &[u8], pos: &mut usize) -> Result<Option<isize>, PointerError> {
    let start = *pos;
    let negative = if bytes.get(*pos) == Some(&b'-') {
        *pos += 1;
        true
    } else {
        false
    };

    let digits_start = *pos;
    let mut acc = 0i128;
    while let Some(&b) = bytes.get(*pos) {
        if !b.is_ascii_digit() {
            break;
        }
        let digit = i128::from(b - b'0');
        let Some(scaled) = acc.checked_mul(10) else {
            return Err(PointerError::InvalidPathSyntax { offset: start });
        };
        acc = if negative {
            match scaled.checked_sub(digit) {
                Some(v) => v,
                None => return Err(PointerError::InvalidPathSyntax { offset: start }),
            }
        } else {
            match scaled.checked_add(digit) {
                Some(v) => v,
                None => return Err(PointerError::InvalidPathSyntax { offset: start }),
            }
        };
        *pos += 1;
    }

    if *pos == digits_start {
        if negative {
            return Err(PointerError::InvalidPathSyntax { offset: start });
        }
        return Ok(None);
    }

    match isize::try_from(acc) {
        Ok(v) => Ok(Some(v)),
        Err(_) => Err(PointerError::InvalidPathSyntax { offset: start }),
    }
}

fn parse_index_or_slice<A: Allocator>(
    bytes: &[u8],
    pos: &mut usize,
) -> Result<Selector<A>, PointerError> {
    let first = parse_optional_isize(bytes, pos)?;
    skip_ascii_spaces(bytes, pos);

    match bytes.get(*pos) {
        Some(b']') => {
            let Some(idx) = first else {
                return Err(PointerError::InvalidPathSyntax { offset: *pos });
            };
            *pos += 1;
            Ok(Selector::Index(idx))
        }
        Some(b':') => {
            *pos += 1;
            skip_ascii_spaces(bytes, pos);
            let end = parse_optional_isize(bytes, pos)?;
            skip_ascii_spaces(bytes, pos);
            let step = if bytes.get(*pos) == Some(&b':') {
                *pos += 1;
                skip_ascii_spaces(bytes, pos);
                let parsed_step = parse_optional_isize(bytes, pos)?;
                skip_ascii_spaces(bytes, pos);
                match parsed_step {
                    Some(s) => s,
                    None => 1,
                }
            } else {
                1
            };
            if bytes.get(*pos) != Some(&b']') {
                return Err(PointerError::InvalidPathSyntax { offset: *pos });
            }
            *pos += 1;
            Ok(Selector::Slice { start: first, end, step })
        }
        _ => Err(PointerError::InvalidPathSyntax { offset: *pos }),
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_data::{Int, Map, Value};
    use blaze_own::testing::heap;
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in};
    use blaze_sim::{Broke, Config, Tape};

    use super::{
        PathLimits, PointerError, escape_token_into, get, get_mut, get_planted, merge_patch,
        query, remove, set,
    };

    fn text<A: Allocator + Copy>(s: &str, alloc: A) -> Value<A> {
        Value::Text(OwnString::from_str_in(s, alloc))
    }

    fn int<A: Allocator>(n: i64) -> Value<A> {
        Value::Int(Int::from_i64(n))
    }

    fn put<A: Allocator + Copy>(m: &mut Map<A>, k: &str, v: Value<A>, alloc: A) {
        let _ = m.insert(OwnString::from_str_in(k, alloc), v);
    }

    #[test]
    fn rfc6901_section5_spec_vectors() {
        let h = heap();
        let alloc = h.alloc();

        let mut foo = OwnVec::new_in(alloc);
        foo.push(text("bar", alloc));
        foo.push(text("baz", alloc));

        let mut root_map = Map::new_in(alloc);
        put(&mut root_map, "foo", Value::List(foo), alloc);
        put(&mut root_map, "", int(0), alloc);
        put(&mut root_map, "a/b", int(1), alloc);
        put(&mut root_map, "c%d", int(2), alloc);
        put(&mut root_map, "e^f", int(3), alloc);
        put(&mut root_map, "g|h", int(4), alloc);
        put(&mut root_map, "i\\j", int(5), alloc);
        put(&mut root_map, "k\"l", int(6), alloc);
        put(&mut root_map, " ", int(7), alloc);
        put(&mut root_map, "m~n", int(8), alloc);
        put(&mut root_map, "~1", int(9), alloc);
        let doc = Value::Map(root_map);

        assert_eq!(get(&doc, ""), Ok(Some(&doc)));
        if let Ok(Some(v)) = get(&doc, "/foo")
            && let Some(items) = v.as_list()
        {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected /foo to resolve to a 2-element list");
        }
        assert_eq!(get(&doc, "/foo/0").ok().flatten().and_then(Value::as_str), Some("bar"));
        assert_eq!(get(&doc, "/foo/1").ok().flatten().and_then(Value::as_str), Some("baz"));
        assert_eq!(get(&doc, "/"), Ok(Some(&int(0))));
        assert_eq!(get(&doc, "/a~1b"), Ok(Some(&int(1))));
        assert_eq!(get(&doc, "/c%d"), Ok(Some(&int(2))));
        assert_eq!(get(&doc, "/e^f"), Ok(Some(&int(3))));
        assert_eq!(get(&doc, "/g|h"), Ok(Some(&int(4))));
        assert_eq!(get(&doc, "/i\\j"), Ok(Some(&int(5))));
        assert_eq!(get(&doc, "/k\"l"), Ok(Some(&int(6))));
        assert_eq!(get(&doc, "/ "), Ok(Some(&int(7))));
        assert_eq!(get(&doc, "/m~0n"), Ok(Some(&int(8))));
        // RFC 6901 §4 ordering: `~01` decodes to `~1`, never `/`.
        assert_eq!(get(&doc, "/~01"), Ok(Some(&int(9))));

        // Strict non-leading-zero array indices and `-` past-the-end.
        assert_eq!(get(&doc, "/foo/00"), Ok(None));
        assert_eq!(get(&doc, "/foo/01"), Ok(None));
        assert_eq!(get(&doc, "/foo/-"), Ok(None));
        assert_eq!(get(&doc, "/foo/2"), Ok(None));

        // Invalid syntax refused with typed errors.
        assert_eq!(get(&doc, "foo"), Err(PointerError::MissingLeadingSlash));
        assert_eq!(get(&doc, "/a~2b"), Err(PointerError::InvalidEscape { offset: 2 }));
        assert_eq!(get(&doc, "/missing/~"), Err(PointerError::InvalidEscape { offset: 9 }));
    }

    #[test]
    fn pointer_get_mut_set_and_remove() {
        let h = heap();
        let alloc = h.alloc();

        let mut list = OwnVec::new_in(alloc);
        list.push(int(10));
        let mut root_map = Map::new_in(alloc);
        put(&mut root_map, "arr", Value::List(list), alloc);
        let mut doc = Value::Map(root_map);

        // Append via `-`, replace via index, mutate via `get_mut`.
        assert_eq!(set(&mut doc, "/arr/-", int(20), alloc), Ok(()));
        assert_eq!(set(&mut doc, "/arr/0", int(11), alloc), Ok(()));
        assert_eq!(set(&mut doc, "/arr/01", int(99), alloc), Err(PointerError::InvalidArrayIndex));
        assert_eq!(
            set(&mut doc, "/arr/5", int(99), alloc),
            Err(PointerError::IndexOutOfBounds { index: 5, len: 2 })
        );

        if let Ok(Some(slot)) = get_mut(&mut doc, "/arr/1") {
            *slot = int(22);
        } else {
            panic!("expected mutable slot at /arr/1");
        }
        assert_eq!(get(&doc, "/arr/0"), Ok(Some(&int(11))));
        assert_eq!(get(&doc, "/arr/1"), Ok(Some(&int(22))));

        // Insert & remove escaped map key `a/b~c`.
        assert_eq!(set(&mut doc, "/a~1b~0c", text("ok", alloc), alloc), Ok(()));
        assert_eq!(get(&doc, "/a~1b~0c").ok().flatten().and_then(Value::as_str), Some("ok"));
        assert_eq!(remove(&mut doc, "/a~1b~0c"), Ok(Some(text("ok", alloc))));
        assert_eq!(get(&doc, "/a~1b~0c"), Ok(None));

        // Remove list element shifts remaining elements.
        assert_eq!(remove(&mut doc, "/arr/0"), Ok(Some(int(11))));
        assert_eq!(get(&doc, "/arr/0"), Ok(Some(&int(22))));
        assert_eq!(remove(&mut doc, ""), Err(PointerError::CannotRemoveRoot));
    }

    #[test]
    fn rfc7396_appendix_a_cases_and_depth_limit() {
        let h = heap();
        let alloc = h.alloc();

        // Helper for single-entry map.
        let map1 = |k: &str, v: Value<_>| {
            let mut m = Map::new_in(alloc);
            put(&mut m, k, v, alloc);
            Value::Map(m)
        };

        // 1. {"a":"b"} + {"a":"c"} => {"a":"c"}
        let r1 = merge_patch(map1("a", text("b", alloc)), map1("a", text("c", alloc)), 16, alloc);
        assert_eq!(r1, Ok(map1("a", text("c", alloc))));

        // 2. {"a":"b"} + {"b":"c"} => {"a":"b","b":"c"}
        let mut exp2 = Map::new_in(alloc);
        put(&mut exp2, "a", text("b", alloc), alloc);
        put(&mut exp2, "b", text("c", alloc), alloc);
        let r2 = merge_patch(map1("a", text("b", alloc)), map1("b", text("c", alloc)), 16, alloc);
        assert_eq!(r2, Ok(Value::Map(exp2)));

        // 3. {"a":"b"} + {"a":null} => {}
        let r3 = merge_patch(map1("a", text("b", alloc)), map1("a", Value::Null), 16, alloc);
        assert_eq!(r3, Ok(Value::Map(Map::new_in(alloc))));

        // 4. {"a":"b","b":"c"} + {"a":null} => {"b":"c"}
        let mut t4 = Map::new_in(alloc);
        put(&mut t4, "a", text("b", alloc), alloc);
        put(&mut t4, "b", text("c", alloc), alloc);
        let r4 = merge_patch(Value::Map(t4), map1("a", Value::Null), 16, alloc);
        assert_eq!(r4, Ok(map1("b", text("c", alloc))));

        // 5. {"a":["b"]} + {"a":"c"} => {"a":"c"}
        let mut l5 = OwnVec::new_in(alloc);
        l5.push(text("b", alloc));
        let r5 = merge_patch(map1("a", Value::List(l5)), map1("a", text("c", alloc)), 16, alloc);
        assert_eq!(r5, Ok(map1("a", text("c", alloc))));

        // 6. {"a":{"b":"c"}} + {"a":{"b":"d","c":null}} => {"a":{"b":"d"}}
        let mut p6_inner = Map::new_in(alloc);
        put(&mut p6_inner, "b", text("d", alloc), alloc);
        put(&mut p6_inner, "c", Value::Null, alloc);
        let r6 = merge_patch(
            map1("a", map1("b", text("c", alloc))),
            map1("a", Value::Map(p6_inner)),
            16,
            alloc,
        );
        assert_eq!(r6, Ok(map1("a", map1("b", text("d", alloc)))));

        // 7. {} + {"a":{"bb":{"ccc":null}}} => {"a":{"bb":{}}}
        let p7 = map1("a", map1("bb", map1("ccc", Value::Null)));
        let r7 = merge_patch(Value::Map(Map::new_in(alloc)), p7, 16, alloc);
        assert_eq!(r7, Ok(map1("a", map1("bb", Value::Map(Map::new_in(alloc))))));

        // 8. {"a":"foo"} + null => null
        let r8 = merge_patch(map1("a", text("foo", alloc)), Value::Null, 16, alloc);
        assert_eq!(r8, Ok(Value::Null));

        // 9. {"e":null} + {"a":1} => {"e":null,"a":1} (existing null in target preserved!)
        let mut exp9 = Map::new_in(alloc);
        put(&mut exp9, "e", Value::Null, alloc);
        put(&mut exp9, "a", int(1), alloc);
        let r9 = merge_patch(map1("e", Value::Null), map1("a", int(1)), 16, alloc);
        assert_eq!(r9, Ok(Value::Map(exp9)));

        // 10. [1,2] + {"a":"b","c":null} => {"a":"b"}
        let mut t10 = OwnVec::new_in(alloc);
        t10.push(int(1));
        t10.push(int(2));
        let mut p10 = Map::new_in(alloc);
        put(&mut p10, "a", text("b", alloc), alloc);
        put(&mut p10, "c", Value::Null, alloc);
        let r10 = merge_patch(Value::List(t10), Value::Map(p10), 16, alloc);
        assert_eq!(r10, Ok(map1("a", text("b", alloc))));

        // Depth limit enforced iteratively.
        let deep_patch = map1("a", map1("b", map1("c", int(1))));
        assert_eq!(
            merge_patch(Value::Map(Map::new_in(alloc)), deep_patch, 2, alloc),
            Err(PointerError::MaxDepthExceeded { limit: 2 })
        );
    }

    #[test]
    fn jsonpath_queries_and_limits() {
        let h = heap();
        let alloc = h.alloc();

        let mut b1 = Map::new_in(alloc);
        put(&mut b1, "author", text("Nigel", alloc), alloc);
        put(&mut b1, "price", int(8), alloc);

        let mut b2 = Map::new_in(alloc);
        put(&mut b2, "author", text("Evelyn", alloc), alloc);
        put(&mut b2, "price", int(12), alloc);

        let mut books = OwnVec::new_in(alloc);
        books.push(Value::Map(b1));
        books.push(Value::Map(b2));

        let mut bike = Map::new_in(alloc);
        put(&mut bike, "price", int(19), alloc);

        let mut store = Map::new_in(alloc);
        put(&mut store, "book", Value::List(books), alloc);
        put(&mut store, "bicycle", Value::Map(bike), alloc);

        let mut items = OwnVec::new_in(alloc);
        for i in 0i64..5 {
            items.push(int(i * 10));
        }

        let mut root_map = Map::new_in(alloc);
        put(&mut root_map, "store", Value::Map(store), alloc);
        put(&mut root_map, "items", Value::List(items), alloc);
        put(&mut root_map, "a.b", text("dotted", alloc), alloc);
        let doc = Value::Map(root_map);

        // $.store.book[*].author
        let authors = query(&doc, "$.store.book[*].author", PathLimits::SHIPPED, alloc);
        assert!(authors.is_ok());
        if let Ok(list) = authors {
            assert_eq!(list.len(), 2);
            assert_eq!(list.first().and_then(|v| v.as_str()), Some("Nigel"));
            assert_eq!(list.get(1).and_then(|v| v.as_str()), Some("Evelyn"));
        }

        // $..price (iterative recursive descent in pre-order)
        let prices = query(&doc, "$..price", PathLimits::SHIPPED, alloc);
        assert!(prices.is_ok());
        if let Ok(list) = prices {
            assert_eq!(list.len(), 3);
            assert_eq!(list.first().and_then(|v| v.as_i64()), Some(8));
            assert_eq!(list.get(1).and_then(|v| v.as_i64()), Some(12));
            assert_eq!(list.get(2).and_then(|v| v.as_i64()), Some(19));
        }

        // $.items[-1]
        let last = query(&doc, "$.items[-1]", PathLimits::SHIPPED, alloc);
        assert!(last.is_ok());
        if let Ok(list) = last {
            assert_eq!(list.len(), 1);
            assert_eq!(list.first().and_then(|v| v.as_i64()), Some(40));
        }

        // $.items[1:3]
        let sliced = query(&doc, "$.items[1:3]", PathLimits::SHIPPED, alloc);
        assert!(sliced.is_ok());
        if let Ok(list) = sliced {
            assert_eq!(list.len(), 2);
            assert_eq!(list.first().and_then(|v| v.as_i64()), Some(10));
            assert_eq!(list.get(1).and_then(|v| v.as_i64()), Some(20));
        }

        // $['a.b']
        let quoted = query(&doc, "$['a.b']", PathLimits::SHIPPED, alloc);
        assert!(quoted.is_ok());
        if let Ok(list) = quoted {
            assert_eq!(list.len(), 1);
            assert_eq!(list.first().and_then(|v| v.as_str()), Some("dotted"));
        }

        // $..* visits all descendants
        let all = query(&doc, "$..*", PathLimits::SHIPPED, alloc);
        assert!(all.is_ok());
        if let Ok(list) = all {
            assert!(list.len() > 10);
        }

        // Bounds enforcement & syntax errors.
        assert_eq!(
            query(&doc, "store.book", PathLimits::SHIPPED, alloc),
            Err(PointerError::InvalidPathSyntax { offset: 0 })
        );
        assert_eq!(
            query(
                &doc,
                "$.store.book[*].author",
                PathLimits { most_segments: 2, ..PathLimits::SHIPPED },
                alloc,
            ),
            Err(PointerError::TooManySegments { limit: 2 })
        );
        assert_eq!(
            query(
                &doc,
                "$..price",
                PathLimits { most_matches: 2, ..PathLimits::SHIPPED },
                alloc,
            ),
            Err(PointerError::TooManyMatches { limit: 2 })
        );
        assert_eq!(
            query(
                &doc,
                "$..*",
                PathLimits { most_steps: 3, ..PathLimits::SHIPPED },
                alloc,
            ),
            Err(PointerError::StepLimitExceeded { limit: 3 })
        );
    }

    // --------------------------------------------------------------------------------------------
    // Property Test + Planted-Bug Control
    // --------------------------------------------------------------------------------------------

    fn draw_tricky_token<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const PIECES: &[&str] = &["", "a", "~", "/", "~0", "~1", "~01", "01", "x/y~z", "café"];
        let count = tape.int_in(1usize..=4, "tok_pieces");
        let mut out = OwnString::new_in(alloc);
        for _ in 0..count {
            if let Some(&p) = tape.pick(PIECES, "tok_piece") {
                out.push_str(p);
            }
        }
        out
    }

    fn check_pointer_and_merge_invariant<A: Allocator + Copy>(
        key: &str,
        plant_wrong_unescape_order: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        // 1. Escape key into a JSON Pointer, `set` a value, then verify `get` round-trips
        //    and the underlying map stores the exact unescaped key.
        let mut ptr = OwnString::from_str_in("/", alloc);
        escape_token_into(key, &mut ptr).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "escape failed: {e}").as_str().into())
        })?;

        let mut doc = Value::Map(Map::new_in(alloc));
        // Also insert a decoy `"/"` key so swapped unescape order collides visibly when key == "~1".
        if key != "/" {
            set(&mut doc, "/~1", text("decoy", alloc), alloc).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "decoy set failed: {e}").as_str().into())
            })?;
        }
        set(&mut doc, ptr.as_str(), int(777), alloc).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "set failed for {key:?}: {e}").as_str().into())
        })?;

        let got = get_planted(&doc, ptr.as_str(), plant_wrong_unescape_order).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "get failed for {key:?}: {e}").as_str().into())
        })?;
        if got != Some(&int(777)) {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "round-trip mismatch for key={key:?} ptr={:?}: got {got:?}",
                    ptr.as_str()
                )
                .as_str()
                .into(),
            ));
        }

        // 2. Verify direct map lookup matches the exact unescaped key.
        if doc.get(key) != Some(&int(777)) {
            return Err(Broke::Oracle(
                format_in!(alloc, "map did not contain unescaped key {key:?}")
                    .as_str()
                    .into(),
            ));
        }

        // 3. Verify RFC 7396 merge_patch with `{ key: null }` deletes `key` while keeping siblings.
        let mut patch_map = Map::new_in(alloc);
        put(&mut patch_map, key, Value::Null, alloc);
        let patched = merge_patch(doc, Value::Map(patch_map), 8, alloc).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "merge_patch failed: {e}").as_str().into())
        })?;
        if patched.get(key).is_some() {
            return Err(Broke::Oracle(
                format_in!(alloc, "RFC 7396 null patch did not delete key {key:?}")
                    .as_str()
                    .into(),
            ));
        }
        if key != "/" && patched.get("/") != Some(&text("decoy", alloc)) {
            return Err(Broke::Oracle(
                format_in!(alloc, "RFC 7396 null patch corrupted sibling key `/`")
                    .as_str()
                    .into(),
            ));
        }

        Ok(())
    }

    /// The invariant: escaping any map key into an RFC 6901 JSON Pointer round-trips through set and get with exact unescape order, and RFC 7396 null merge patches always delete the targeted key without disturbing siblings.
    #[test]
    fn property_pointer_round_trip_and_merge_patch_null_deletion() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let key = draw_tricky_token(tape, h.alloc());
            check_pointer_and_merge_invariant(key.as_str(), false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_unescape_order_bug() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let key = draw_tricky_token(tape, h.alloc());
            check_pointer_and_merge_invariant(key.as_str(), plant, h.alloc())
        });
    }
}
