//! Pure `#![no_std]` sans-IO HTTP URL router backed by a flat segment-trie arena.
//!
//! - Supports static segments (`/users/profile`), named single-segment parameters
//!   (`/users/:id/posts/:post_id`), and trailing catch-all wildcards (`/files/*path`
//!   capturing the remaining path without a leading `/`, including empty or multi-segment
//!   remainders).
//! - Deterministic precedence: **static > `:param` > `*wildcard`**, with 100% iterative
//!   backtracking across candidate branches (zero recursion during both insertion and
//!   resolution).
//! - Distinguishes `200`-style [`RouteOutcome::Matched`], `405`-style
//!   [`RouteOutcome::MethodNotAllowed`] (carrying the exact [`MethodSet`] for the `Allow`
//!   header), and `404`-style [`RouteOutcome::NotFound`].
//! - Strips `?query` and `#fragment` components automatically during resolution.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

const METHOD_COUNT: usize = 9;

/// Standard HTTP request methods supported by [`Router`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Patch,
    Options,
    Trace,
    Connect,
}

impl Method {
    /// All nine standard HTTP methods in canonical [`MethodSet::format_allow_into`] order.
    pub const ALL: [Method; METHOD_COUNT] = [
        Method::Get,
        Method::Head,
        Method::Post,
        Method::Put,
        Method::Delete,
        Method::Patch,
        Method::Options,
        Method::Trace,
        Method::Connect,
    ];

    /// Parse an exact uppercase ASCII HTTP method token (RFC 9110 §9.1).
    #[inline]
    #[must_use]
    pub fn from_str_ascii(s: &str) -> Option<Self> {
        match s {
            "GET" => Some(Method::Get),
            "HEAD" => Some(Method::Head),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "DELETE" => Some(Method::Delete),
            "PATCH" => Some(Method::Patch),
            "OPTIONS" => Some(Method::Options),
            "TRACE" => Some(Method::Trace),
            "CONNECT" => Some(Method::Connect),
            _ => None,
        }
    }

    /// Canonical uppercase HTTP method name.
    #[inline]
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match *self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
        }
    }

    /// Single-bit mask identifying this method inside a [`MethodSet`].
    #[inline]
    #[must_use]
    pub const fn bit(&self) -> u16 {
        match *self {
            Method::Get => 1 << 0,
            Method::Head => 1 << 1,
            Method::Post => 1 << 2,
            Method::Put => 1 << 3,
            Method::Delete => 1 << 4,
            Method::Patch => 1 << 5,
            Method::Options => 1 << 6,
            Method::Trace => 1 << 7,
            Method::Connect => 1 << 8,
        }
    }

    #[inline]
    const fn index(self) -> usize {
        match self {
            Method::Get => 0,
            Method::Head => 1,
            Method::Post => 2,
            Method::Put => 3,
            Method::Delete => 4,
            Method::Patch => 5,
            Method::Options => 6,
            Method::Trace => 7,
            Method::Connect => 8,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compact bitmask of HTTP [`Method`]s, suitable for formatting RFC 9110 `Allow` headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MethodSet(u16);

impl MethodSet {
    /// Empty method set.
    pub const EMPTY: Self = Self(0);

    /// Method set containing all nine standard HTTP methods.
    pub const ALL: Self = Self((1u16 << 9) - 1);

    /// Construct an empty [`MethodSet`].
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self::EMPTY
    }

    /// Construct a [`MethodSet`] containing only `method`.
    #[inline]
    #[must_use]
    pub const fn single(method: Method) -> Self {
        Self(method.bit())
    }

    /// Return `true` if `method` is present in this set.
    #[inline]
    #[must_use]
    pub const fn contains(self, method: Method) -> bool {
        (self.0 & method.bit()) != 0
    }

    /// Insert `method` into this set.
    #[inline]
    pub fn insert(&mut self, method: Method) {
        self.0 |= method.bit();
    }

    /// Return the union of two method sets.
    #[inline]
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Return `true` if no methods are set.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Raw bitmask representation.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Write a comma-space separated RFC 9110 `Allow` header value (e.g. `"GET, POST"`).
    pub fn format_allow_into<W: fmt::Write>(self, out: &mut W) -> fmt::Result {
        let mut first = true;
        for method in Method::ALL {
            if self.contains(method) {
                if !first {
                    out.write_str(", ")?;
                }
                out.write_str(method.as_str())?;
                first = false;
            }
        }
        Ok(())
    }
}

impl fmt::Display for MethodSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.format_allow_into(f)
    }
}

/// Typed failure returned when inserting a route into [`Router`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouterError {
    /// Route pattern did not begin with `/`.
    MissingLeadingSlash,
    /// Named parameter segment `:` had an empty parameter name.
    EmptyParamName,
    /// Catch-all wildcard segment `*` had an empty parameter name.
    EmptyWildcardName,
    /// A `*wildcard` segment appeared before the end of the route pattern.
    WildcardNotTrailing,
    /// A `:param` segment at the same tree level used a different name than an existing route.
    ConflictingParamName,
    /// A `*wildcard` segment at the same tree level used a different name than an existing route.
    ConflictingWildcardName,
    /// A handler was already registered for the same HTTP method and pattern.
    DuplicateRoute,
    /// Attempted to register a handler with an empty [`MethodSet`].
    EmptyMethodSet,
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for RouterError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RouterError::MissingLeadingSlash => {
                f.write_str("bstd::router: route pattern must start with `/`")
            }
            RouterError::EmptyParamName => {
                f.write_str("bstd::router: parameter segment `:` requires a non-empty name")
            }
            RouterError::EmptyWildcardName => {
                f.write_str("bstd::router: wildcard segment `*` requires a non-empty name")
            }
            RouterError::WildcardNotTrailing => {
                f.write_str("bstd::router: `*wildcard` segment must be the final segment")
            }
            RouterError::ConflictingParamName => f.write_str(
                "bstd::router: conflicting `:param` name at the same route tree level",
            ),
            RouterError::ConflictingWildcardName => f.write_str(
                "bstd::router: conflicting `*wildcard` name at the same route tree level",
            ),
            RouterError::DuplicateRoute => {
                f.write_str("bstd::router: duplicate route for the same HTTP method and pattern")
            }
            RouterError::EmptyMethodSet => {
                f.write_str("bstd::router: cannot register a route with an empty MethodSet")
            }
            RouterError::Alloc(err) => write!(f, "bstd::router: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for RouterError {}

/// Outcome of resolving an HTTP method and request target against a [`Router`].
pub enum RouteOutcome<'r, 'p, T, A: Allocator> {
    /// The request matched a registered route for the requested method.
    Matched {
        handler: &'r T,
        params: Vec<(&'r str, &'p str), A>,
    },
    /// The path matched a registered route, but only for other HTTP methods.
    MethodNotAllowed { allowed: MethodSet },
    /// No registered route matched the request path.
    NotFound,
}

impl<'r1, 'p1, 'r2, 'p2, T: PartialEq, A: Allocator, B: Allocator>
    PartialEq<RouteOutcome<'r2, 'p2, T, B>> for RouteOutcome<'r1, 'p1, T, A>
{
    fn eq(&self, other: &RouteOutcome<'r2, 'p2, T, B>) -> bool {
        match (self, other) {
            (
                RouteOutcome::Matched { handler: h1, params: p1 },
                RouteOutcome::Matched { handler: h2, params: p2 },
            ) => h1 == h2 && p1.as_slice() == p2.as_slice(),
            (
                RouteOutcome::MethodNotAllowed { allowed: a1 },
                RouteOutcome::MethodNotAllowed { allowed: a2 },
            ) => a1 == a2,
            (RouteOutcome::NotFound, RouteOutcome::NotFound) => true,
            (RouteOutcome::Matched { .. }, RouteOutcome::MethodNotAllowed { .. })
            | (RouteOutcome::Matched { .. }, RouteOutcome::NotFound)
            | (RouteOutcome::MethodNotAllowed { .. }, RouteOutcome::Matched { .. })
            | (RouteOutcome::MethodNotAllowed { .. }, RouteOutcome::NotFound)
            | (RouteOutcome::NotFound, RouteOutcome::Matched { .. })
            | (RouteOutcome::NotFound, RouteOutcome::MethodNotAllowed { .. }) => false,
        }
    }
}

impl<T: Eq, A: Allocator> Eq for RouteOutcome<'_, '_, T, A> {}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for RouteOutcome<'_, '_, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteOutcome::Matched { handler, params } => f
                .debug_struct("Matched")
                .field("handler", handler)
                .field("params", &params.as_slice())
                .finish(),
            RouteOutcome::MethodNotAllowed { allowed } => f
                .debug_struct("MethodNotAllowed")
                .field("allowed", allowed)
                .finish(),
            RouteOutcome::NotFound => f.write_str("NotFound"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternSeg<'a> {
    Static(&'a str),
    Param(&'a str),
    Wildcard(&'a str),
}

struct StaticEdge<A: Allocator> {
    segment: String<A>,
    child: usize,
}

struct ParamEdge<A: Allocator> {
    name: String<A>,
    child: usize,
}

struct WildcardEdge<A: Allocator> {
    name: String<A>,
    child: usize,
}

struct Node<A: Allocator> {
    statics: Vec<StaticEdge<A>, A>,
    param: Option<ParamEdge<A>>,
    wildcard: Option<WildcardEdge<A>>,
    handlers: [Option<usize>; METHOD_COUNT],
    methods: MethodSet,
}

impl<A: Allocator> Node<A> {
    fn new_in(alloc: A) -> Self {
        Self {
            statics: Vec::new_in(alloc),
            param: None,
            wildcard: None,
            handlers: [None; METHOD_COUNT],
            methods: MethodSet::EMPTY,
        }
    }
}

impl<A: Allocator> fmt::Debug for Node<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("static_count", &self.statics.len())
            .field("has_param", &self.param.is_some())
            .field("has_wildcard", &self.wildcard.is_some())
            .field("methods", &self.methods)
            .finish()
    }
}

/// Arena-allocated, 100% iterative sans-IO HTTP URL router.
pub struct Router<T, A: Allocator> {
    nodes: Vec<Node<A>, A>,
    handlers: Vec<T, A>,
    route_count: usize,
    alloc: A,
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Router<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("node_count", &self.nodes.len())
            .field("handler_count", &self.handlers.len())
            .field("route_count", &self.route_count)
            .finish()
    }
}

impl<T, A: Allocator> Router<T, A> {
    /// Number of `(method, pattern)` bindings registered in this router.
    #[inline]
    #[must_use]
    pub fn route_count(&self) -> usize {
        self.route_count
    }

    /// Return `true` if no routes have been registered yet.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.route_count == 0
    }
}

impl<T, A: Allocator + Copy> Router<T, A> {
    /// Construct an empty [`Router`] backed by `alloc`.
    #[inline]
    #[must_use]
    pub fn new_in(alloc: A) -> Self {
        Self {
            nodes: Vec::new_in(alloc),
            handlers: Vec::new_in(alloc),
            route_count: 0,
            alloc,
        }
    }

    /// Register `handler` for a single HTTP `method` and `pattern`.
    pub fn insert(
        &mut self,
        method: Method,
        pattern: &str,
        handler: T,
    ) -> Result<(), RouterError> {
        self.insert_methods(MethodSet::single(method), pattern, handler)
    }

    /// Register a single `handler` for all nine standard HTTP methods on `pattern`.
    pub fn insert_any(&mut self, pattern: &str, handler: T) -> Result<(), RouterError> {
        self.insert_methods(MethodSet::ALL, pattern, handler)
    }

    /// Register a single `handler` for every method in `methods` on `pattern`.
    pub fn insert_methods(
        &mut self,
        methods: MethodSet,
        pattern: &str,
        handler: T,
    ) -> Result<(), RouterError> {
        if methods.is_empty() {
            return Err(RouterError::EmptyMethodSet);
        }

        // Phase 1: Validate pattern syntax completely before mutating the trie.
        let mut parsed: Vec<PatternSeg<'_>, A> = Vec::new_in(self.alloc);
        parse_pattern(pattern, &mut parsed)?;

        // Phase 2: Read-only conflict and duplicate validation along any existing prefix.
        if !self.nodes.is_empty() {
            let mut cur_idx = 0usize;
            let mut reached_existing = true;
            for &seg in parsed.as_slice() {
                let Some(node) = self.nodes.get(cur_idx) else {
                    reached_existing = false;
                    break;
                };
                match seg {
                    PatternSeg::Static(lit) => {
                        match node
                            .statics
                            .as_slice()
                            .binary_search_by(|e| e.segment.as_str().cmp(lit))
                        {
                            Ok(pos) => {
                                if let Some(edge) = node.statics.get(pos) {
                                    cur_idx = edge.child;
                                } else {
                                    reached_existing = false;
                                    break;
                                }
                            }
                            Err(_) => {
                                reached_existing = false;
                                break;
                            }
                        }
                    }
                    PatternSeg::Param(name) => match &node.param {
                        Some(edge) => {
                            if edge.name.as_str() != name {
                                return Err(RouterError::ConflictingParamName);
                            }
                            cur_idx = edge.child;
                        }
                        None => {
                            reached_existing = false;
                            break;
                        }
                    },
                    PatternSeg::Wildcard(name) => match &node.wildcard {
                        Some(edge) => {
                            if edge.name.as_str() != name {
                                return Err(RouterError::ConflictingWildcardName);
                            }
                            cur_idx = edge.child;
                        }
                        None => {
                            reached_existing = false;
                            break;
                        }
                    },
                }
            }

            if reached_existing
                && let Some(target_node) = self.nodes.get(cur_idx)
                && (target_node.methods.bits() & methods.bits()) != 0
            {
                return Err(RouterError::DuplicateRoute);
            }
        }

        // Phase 3: Ensure root node exists, walk/create path, and bind handler.
        if self.nodes.is_empty() {
            self.nodes
                .try_push(Node::new_in(self.alloc))
                .map_err(|r| RouterError::Alloc(r.why))?;
        }

        let mut cur_idx = 0usize;
        for &seg in parsed.as_slice() {
            match seg {
                PatternSeg::Static(lit) => {
                    let lookup = match self.nodes.get(cur_idx) {
                        Some(node) => node
                            .statics
                            .as_slice()
                            .binary_search_by(|e| e.segment.as_str().cmp(lit)),
                        None => return Err(RouterError::MissingLeadingSlash),
                    };
                    match lookup {
                        Ok(pos) => {
                            if let Some(node) = self.nodes.get(cur_idx)
                                && let Some(edge) = node.statics.get(pos)
                            {
                                cur_idx = edge.child;
                            }
                        }
                        Err(insert_pos) => {
                            let seg_owned = String::try_from_str_in(lit, self.alloc)?;
                            let new_idx = self.nodes.len();
                            self.nodes
                                .try_push(Node::new_in(self.alloc))
                                .map_err(|r| RouterError::Alloc(r.why))?;
                            if let Some(node) = self.nodes.get_mut(cur_idx) {
                                node.statics
                                    .try_push(StaticEdge { segment: seg_owned, child: new_idx })
                                    .map_err(|r| RouterError::Alloc(r.why))?;
                                if let Some(tail) = node.statics.as_mut_slice().get_mut(insert_pos..)
                                {
                                    tail.rotate_right(1);
                                }
                            }
                            cur_idx = new_idx;
                        }
                    }
                }
                PatternSeg::Param(name) => {
                    let existing = match self.nodes.get(cur_idx) {
                        Some(node) => match &node.param {
                            Some(edge) => Some(edge.child),
                            None => None,
                        },
                        None => None,
                    };
                    match existing {
                        Some(child) => {
                            cur_idx = child;
                        }
                        None => {
                            let name_owned = String::try_from_str_in(name, self.alloc)?;
                            let new_idx = self.nodes.len();
                            self.nodes
                                .try_push(Node::new_in(self.alloc))
                                .map_err(|r| RouterError::Alloc(r.why))?;
                            if let Some(node) = self.nodes.get_mut(cur_idx) {
                                node.param = Some(ParamEdge { name: name_owned, child: new_idx });
                            }
                            cur_idx = new_idx;
                        }
                    }
                }
                PatternSeg::Wildcard(name) => {
                    let existing = match self.nodes.get(cur_idx) {
                        Some(node) => match &node.wildcard {
                            Some(edge) => Some(edge.child),
                            None => None,
                        },
                        None => None,
                    };
                    match existing {
                        Some(child) => {
                            cur_idx = child;
                        }
                        None => {
                            let name_owned = String::try_from_str_in(name, self.alloc)?;
                            let new_idx = self.nodes.len();
                            self.nodes
                                .try_push(Node::new_in(self.alloc))
                                .map_err(|r| RouterError::Alloc(r.why))?;
                            if let Some(node) = self.nodes.get_mut(cur_idx) {
                                node.wildcard =
                                    Some(WildcardEdge { name: name_owned, child: new_idx });
                            }
                            cur_idx = new_idx;
                        }
                    }
                }
            }
        }

        let handler_idx = self.handlers.len();
        self.handlers
            .try_push(handler)
            .map_err(|r| RouterError::Alloc(r.why))?;

        let mut added = 0usize;
        if let Some(node) = self.nodes.get_mut(cur_idx) {
            for method in Method::ALL {
                if methods.contains(method) {
                    if let Some(slot) = node.handlers.get_mut(method.index()) {
                        *slot = Some(handler_idx);
                    }
                    node.methods.insert(method);
                    added += 1;
                }
            }
        }
        self.route_count += added;
        Ok(())
    }

    /// Resolve `method` and request target `path` (stripping any `?query` or `#fragment`).
    pub fn resolve<'r, 'p>(
        &'r self,
        method: Method,
        path: &'p str,
        alloc: A,
    ) -> Result<RouteOutcome<'r, 'p, T, A>, AllocFailure> {
        self.resolve_planted(method, path, alloc, false)
    }

    fn resolve_planted<'r, 'p>(
        &'r self,
        method: Method,
        path: &'p str,
        alloc: A,
        plant_swap_static_param: bool,
    ) -> Result<RouteOutcome<'r, 'p, T, A>, AllocFailure> {
        if self.nodes.is_empty() {
            return Ok(RouteOutcome::NotFound);
        }

        let clean = strip_query_and_fragment(path);
        let Some(trimmed) = clean.strip_prefix('/') else {
            return Ok(RouteOutcome::NotFound);
        };

        let initial_tail = if trimmed.is_empty() {
            PathTail::End
        } else {
            PathTail::More(trimmed)
        };

        let mut stack: Vec<SearchFrame<'p>, A> = Vec::new_in(alloc);
        let mut params: Vec<(&'r str, &'p str), A> = Vec::new_in(alloc);
        stack
            .try_push(SearchFrame {
                node_idx: 0,
                tail: initial_tail,
                params_len: 0,
                stage: BranchStage::First,
            })
            .map_err(|r| r.why)?;

        loop {
            let Some(&frame) = stack.last() else {
                break;
            };
            params.truncate(frame.params_len);

            if frame.stage == BranchStage::Done {
                let _ = stack.pop();
                continue;
            }

            let Some(node) = self.nodes.get(frame.node_idx) else {
                let _ = stack.pop();
                continue;
            };

            match frame.tail {
                PathTail::End => {
                    // Leaf at end of path: exact route beats empty wildcard catch-all.
                    let _ = stack.pop();
                    if !node.methods.is_empty() {
                        return Ok(finish_node(node, &self.handlers, method, params));
                    }
                    if let Some(ref wc) = node.wildcard
                        && let Some(wc_node) = self.nodes.get(wc.child)
                        && !wc_node.methods.is_empty()
                    {
                        if wc_node.methods.contains(method) {
                            params
                                .try_push((wc.name.as_str(), ""))
                                .map_err(|r| r.why)?;
                        }
                        return Ok(finish_node(wc_node, &self.handlers, method, params));
                    }
                }
                PathTail::More(rest) => {
                    if let Some(top_mut) = stack.last_mut() {
                        top_mut.stage = frame.stage.next();
                    }
                    let (seg, next_tail) = split_next_segment(rest);

                    let action = match (frame.stage, plant_swap_static_param) {
                        (BranchStage::First, false) | (BranchStage::Second, true) => {
                            BranchAction::Static
                        }
                        (BranchStage::Second, false) | (BranchStage::First, true) => {
                            BranchAction::Param
                        }
                        (BranchStage::Third, false) | (BranchStage::Third, true) => {
                            BranchAction::Wildcard
                        }
                        (BranchStage::Done, false) | (BranchStage::Done, true) => {
                            BranchAction::Skip
                        }
                    };

                    match action {
                        BranchAction::Static => {
                            if let Ok(edge_pos) = node
                                .statics
                                .as_slice()
                                .binary_search_by(|e| e.segment.as_str().cmp(seg))
                                && let Some(edge) = node.statics.get(edge_pos)
                            {
                                stack
                                    .try_push(SearchFrame {
                                        node_idx: edge.child,
                                        tail: next_tail,
                                        params_len: params.len(),
                                        stage: BranchStage::First,
                                    })
                                    .map_err(|r| r.why)?;
                            }
                        }
                        BranchAction::Param => {
                            if !seg.is_empty()
                                && let Some(ref p_edge) = node.param
                            {
                                params
                                    .try_push((p_edge.name.as_str(), seg))
                                    .map_err(|r| r.why)?;
                                stack
                                    .try_push(SearchFrame {
                                        node_idx: p_edge.child,
                                        tail: next_tail,
                                        params_len: params.len(),
                                        stage: BranchStage::First,
                                    })
                                    .map_err(|r| r.why)?;
                            }
                        }
                        BranchAction::Wildcard => {
                            if let Some(ref wc_edge) = node.wildcard
                                && let Some(wc_node) = self.nodes.get(wc_edge.child)
                                && !wc_node.methods.is_empty()
                            {
                                if wc_node.methods.contains(method) {
                                    params
                                        .try_push((wc_edge.name.as_str(), rest))
                                        .map_err(|r| r.why)?;
                                }
                                return Ok(finish_node(wc_node, &self.handlers, method, params));
                            }
                        }
                        BranchAction::Skip => {}
                    }
                }
            }
        }

        Ok(RouteOutcome::NotFound)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathTail<'p> {
    End,
    More(&'p str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchStage {
    First,
    Second,
    Third,
    Done,
}

impl BranchStage {
    #[inline]
    const fn next(self) -> Self {
        match self {
            BranchStage::First => BranchStage::Second,
            BranchStage::Second => BranchStage::Third,
            BranchStage::Third => BranchStage::Done,
            BranchStage::Done => BranchStage::Done,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchAction {
    Static,
    Param,
    Wildcard,
    Skip,
}

#[derive(Clone, Copy)]
struct SearchFrame<'p> {
    node_idx: usize,
    tail: PathTail<'p>,
    params_len: usize,
    stage: BranchStage,
}

fn finish_node<'r, 'p, T, A: Allocator>(
    node: &Node<A>,
    handlers: &'r Vec<T, A>,
    method: Method,
    params: Vec<(&'r str, &'p str), A>,
) -> RouteOutcome<'r, 'p, T, A> {
    if let Some(Some(handler_idx)) = node.handlers.get(method.index())
        && let Some(handler) = handlers.get(*handler_idx)
    {
        return RouteOutcome::Matched { handler, params };
    }
    RouteOutcome::MethodNotAllowed { allowed: node.methods }
}

fn strip_query_and_fragment(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'?' || b == b'#' {
            return match path.get(..i) {
                Some(prefix) => prefix,
                None => "",
            };
        }
        i += 1;
    }
    path
}

fn split_next_segment(rest: &str) -> (&str, PathTail<'_>) {
    match rest.find('/') {
        Some(slash) => {
            let seg = match rest.get(..slash) {
                Some(s) => s,
                None => "",
            };
            let after = match rest.get(slash + 1..) {
                Some(a) => a,
                None => "",
            };
            (seg, PathTail::More(after))
        }
        None => (rest, PathTail::End),
    }
}

fn parse_pattern<'a, A: Allocator>(
    pattern: &'a str,
    out: &mut Vec<PatternSeg<'a>, A>,
) -> Result<(), RouterError> {
    let Some(rest) = pattern.strip_prefix('/') else {
        return Err(RouterError::MissingLeadingSlash);
    };
    if rest.is_empty() {
        return Ok(());
    }

    let mut saw_wildcard = false;
    for raw in rest.split('/') {
        if saw_wildcard {
            return Err(RouterError::WildcardNotTrailing);
        }
        if let Some(name) = raw.strip_prefix(':') {
            if name.is_empty() {
                return Err(RouterError::EmptyParamName);
            }
            out.try_push(PatternSeg::Param(name))
                .map_err(|r| RouterError::Alloc(r.why))?;
        } else if let Some(name) = raw.strip_prefix('*') {
            if name.is_empty() {
                return Err(RouterError::EmptyWildcardName);
            }
            saw_wildcard = true;
            out.try_push(PatternSeg::Wildcard(name))
                .map_err(|r| RouterError::Alloc(r.why))?;
        } else {
            out.try_push(PatternSeg::Static(raw))
                .map_err(|r| RouterError::Alloc(r.why))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;
    use core::cmp::Ordering;

    use blaze_own::testing::heap;
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in};
    use blaze_sim::{Broke, Config, Tape};

    use super::{Method, MethodSet, PatternSeg, RouteOutcome, Router, RouterError, parse_pattern};

    #[test]
    fn method_parsing_and_allow_header() {
        let h = heap();
        let alloc = h.alloc();

        for method in Method::ALL {
            assert_eq!(Method::from_str_ascii(method.as_str()), Some(method));
        }
        assert_eq!(Method::from_str_ascii("get"), None);
        assert_eq!(Method::from_str_ascii("BREW"), None);

        let mut set = MethodSet::empty();
        assert!(set.is_empty());
        set.insert(Method::Post);
        set.insert(Method::Get);
        set.insert(Method::Delete);
        assert!(!set.is_empty());
        assert!(set.contains(Method::Get));
        assert!(set.contains(Method::Post));
        assert!(set.contains(Method::Delete));
        assert!(!set.contains(Method::Put));

        let mut header = OwnString::new_in(alloc);
        assert!(set.format_allow_into(&mut header).is_ok());
        assert_eq!(header.as_str(), "GET, POST, DELETE");
    }

    #[test]
    fn static_param_wildcard_precedence_and_query_stripping() {
        let h = heap();
        let alloc = h.alloc();

        let mut router = Router::new_in(alloc);
        assert!(router.is_empty());
        assert_eq!(router.insert(Method::Get, "/", "root"), Ok(()));
        assert_eq!(router.insert(Method::Get, "/users/profile", "static_profile"), Ok(()));
        assert_eq!(router.insert(Method::Get, "/users/:id", "user_by_id"), Ok(()));
        assert_eq!(
            router.insert(Method::Get, "/users/:id/posts/:post_id", "user_post"),
            Ok(())
        );
        assert_eq!(router.insert(Method::Get, "/files/*path", "files_wc"), Ok(()));
        assert_eq!(router.route_count(), 5);

        // Root route with query and fragment stripped.
        match router.resolve(Method::Get, "/?debug=1#top", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "root");
                assert!(params.is_empty());
            }
            other => panic!("expected root match, got {other:?}"),
        }

        // Static beats `:param`.
        match router.resolve(Method::Get, "/users/profile?tab=bio#avatar", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "static_profile");
                assert!(params.is_empty());
            }
            other => panic!("expected static_profile, got {other:?}"),
        }

        // `:param` matches single segment.
        match router.resolve(Method::Get, "/users/42", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "user_by_id");
                assert_eq!(params.as_slice(), &[("id", "42")]);
            }
            other => panic!("expected user_by_id, got {other:?}"),
        }

        // `:param` does not match an empty segment `/users/`.
        assert_eq!(router.resolve(Method::Get, "/users/", alloc), Ok(RouteOutcome::NotFound));

        // Multi-param route.
        match router.resolve(Method::Get, "/users/ada/posts/99?fmt=json", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "user_post");
                assert_eq!(params.as_slice(), &[("id", "ada"), ("post_id", "99")]);
            }
            other => panic!("expected user_post, got {other:?}"),
        }

        // Wildcard captures multi-segment remainder, empty remainder after `/`, and bare prefix.
        match router.resolve(Method::Get, "/files/docs/a/b.txt#sec", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "files_wc");
                assert_eq!(params.as_slice(), &[("path", "docs/a/b.txt")]);
            }
            other => panic!("expected files_wc multi, got {other:?}"),
        }
        match router.resolve(Method::Get, "/files/", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "files_wc");
                assert_eq!(params.as_slice(), &[("path", "")]);
            }
            other => panic!("expected files_wc trailing slash, got {other:?}"),
        }
        match router.resolve(Method::Get, "/files", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "files_wc");
                assert_eq!(params.as_slice(), &[("path", "")]);
            }
            other => panic!("expected files_wc bare prefix, got {other:?}"),
        }
    }

    #[test]
    fn iterative_backtracking_from_static_prefix_into_param_and_wildcard() {
        let h = heap();
        let alloc = h.alloc();

        let mut router = Router::new_in(alloc);
        assert_eq!(router.insert(Method::Get, "/users/me/profile", "me_profile"), Ok(()));
        assert_eq!(router.insert(Method::Get, "/users/:id/settings", "id_settings"), Ok(()));
        assert_eq!(router.insert(Method::Get, "/users/*rest", "users_wc"), Ok(()));

        // Must backtrack out of static `/users/me` into `/users/:id/settings`.
        match router.resolve(Method::Get, "/users/me/settings", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "id_settings");
                assert_eq!(params.as_slice(), &[("id", "me")]);
            }
            other => panic!("expected id_settings, got {other:?}"),
        }

        // Must backtrack out of both static `/users/me` and `:id` into `/users/*rest` without leaking `id`.
        match router.resolve(Method::Get, "/users/me/unknown/deep", alloc) {
            Ok(RouteOutcome::Matched { handler, params }) => {
                assert_eq!(*handler, "users_wc");
                assert_eq!(params.as_slice(), &[("rest", "me/unknown/deep")]);
            }
            other => panic!("expected users_wc, got {other:?}"),
        }
    }

    #[test]
    fn method_not_allowed_vs_not_found_and_insert_any() {
        let h = heap();
        let alloc = h.alloc();

        let mut router = Router::new_in(alloc);
        assert_eq!(router.insert(Method::Get, "/items/:id", "get_item"), Ok(()));
        assert_eq!(router.insert(Method::Put, "/items/:id", "put_item"), Ok(()));
        assert_eq!(router.insert_any("/health", "health_any"), Ok(()));

        let mut expected_allow = MethodSet::empty();
        expected_allow.insert(Method::Get);
        expected_allow.insert(Method::Put);

        assert_eq!(
            router.resolve(Method::Delete, "/items/123", alloc),
            Ok(RouteOutcome::MethodNotAllowed { allowed: expected_allow })
        );
        assert_eq!(router.resolve(Method::Get, "/missing", alloc), Ok(RouteOutcome::NotFound));
        assert_eq!(router.resolve(Method::Get, "relative/path", alloc), Ok(RouteOutcome::NotFound));

        for method in Method::ALL {
            match router.resolve(method, "/health", alloc) {
                Ok(RouteOutcome::Matched { handler, params }) => {
                    assert_eq!(*handler, "health_any");
                    assert!(params.is_empty());
                }
                other => panic!("expected health_any for {method}, got {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_patterns_and_conflicts_refused() {
        let h = heap();
        let alloc = h.alloc();

        let mut router = Router::new_in(alloc);
        assert_eq!(router.insert(Method::Get, "", 1usize), Err(RouterError::MissingLeadingSlash));
        assert_eq!(
            router.insert(Method::Get, "users", 1usize),
            Err(RouterError::MissingLeadingSlash)
        );
        assert_eq!(
            router.insert(Method::Get, "/users/:", 1usize),
            Err(RouterError::EmptyParamName)
        );
        assert_eq!(
            router.insert(Method::Get, "/users/:/posts", 1usize),
            Err(RouterError::EmptyParamName)
        );
        assert_eq!(
            router.insert(Method::Get, "/files/*", 1usize),
            Err(RouterError::EmptyWildcardName)
        );
        assert_eq!(
            router.insert(Method::Get, "/files/*path/more", 1usize),
            Err(RouterError::WildcardNotTrailing)
        );
        assert_eq!(
            router.insert_methods(MethodSet::EMPTY, "/ok", 1usize),
            Err(RouterError::EmptyMethodSet)
        );

        assert_eq!(router.insert(Method::Get, "/users/:id/posts", 10usize), Ok(()));
        assert_eq!(
            router.insert(Method::Get, "/users/:id/posts", 11usize),
            Err(RouterError::DuplicateRoute)
        );
        assert_eq!(
            router.insert(Method::Post, "/users/:user_id/settings", 12usize),
            Err(RouterError::ConflictingParamName)
        );
        assert_eq!(
            router.insert_any("/users/:id/posts", 13usize),
            Err(RouterError::DuplicateRoute)
        );

        assert_eq!(router.insert(Method::Get, "/files/*a", 20usize), Ok(()));
        assert_eq!(
            router.insert(Method::Post, "/files/*b", 21usize),
            Err(RouterError::ConflictingWildcardName)
        );
        assert_eq!(router.route_count(), 2);
    }

    // --------------------------------------------------------------------------------------------
    // Linear Brute-Force Reference Oracle + Property Test + Planted-Bug Control
    // --------------------------------------------------------------------------------------------

    struct OracleRoute<A: Allocator> {
        method: Method,
        pattern: OwnString<A>,
        handler: usize,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MatchRankSeg {
        ExactEnd,
        Static,
        Param,
        Wildcard,
    }

    impl MatchRankSeg {
        const fn priority(self) -> u8 {
            match self {
                MatchRankSeg::ExactEnd => 3,
                MatchRankSeg::Static => 2,
                MatchRankSeg::Param => 1,
                MatchRankSeg::Wildcard => 0,
            }
        }
    }

    fn compare_ranks(a: &[MatchRankSeg], b: &[MatchRankSeg]) -> Ordering {
        let mut i = 0usize;
        while let (Some(&ra), Some(&rb)) = (a.get(i), b.get(i)) {
            let ord = ra.priority().cmp(&rb.priority());
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
        }
        a.len().cmp(&b.len())
    }

    fn linear_match<'r, 'p, A: Allocator + Copy>(
        pattern: &'r str,
        path: &'p str,
        alloc: A,
    ) -> Option<(OwnVec<MatchRankSeg, A>, OwnVec<(&'r str, &'p str), A>)> {
        let clean = super::strip_query_and_fragment(path);
        let trimmed = clean.strip_prefix('/')?;
        let mut pat_segs: OwnVec<PatternSeg<'r>, A> = OwnVec::new_in(alloc);
        if parse_pattern(pattern, &mut pat_segs).is_err() {
            return None;
        }

        let mut rank: OwnVec<MatchRankSeg, A> = OwnVec::new_in(alloc);
        let mut params: OwnVec<(&'r str, &'p str), A> = OwnVec::new_in(alloc);
        let mut tail = if trimmed.is_empty() {
            super::PathTail::End
        } else {
            super::PathTail::More(trimmed)
        };

        for &pseg in pat_segs.as_slice() {
            match pseg {
                PatternSeg::Static(lit) => match tail {
                    super::PathTail::End => return None,
                    super::PathTail::More(rest) => {
                        let (seg, next) = super::split_next_segment(rest);
                        if seg != lit {
                            return None;
                        }
                        rank.push(MatchRankSeg::Static);
                        tail = next;
                    }
                },
                PatternSeg::Param(name) => match tail {
                    super::PathTail::End => return None,
                    super::PathTail::More(rest) => {
                        let (seg, next) = super::split_next_segment(rest);
                        if seg.is_empty() {
                            return None;
                        }
                        rank.push(MatchRankSeg::Param);
                        params.push((name, seg));
                        tail = next;
                    }
                },
                PatternSeg::Wildcard(name) => {
                    let captured = match tail {
                        super::PathTail::End => "",
                        super::PathTail::More(rest) => rest,
                    };
                    rank.push(MatchRankSeg::Wildcard);
                    params.push((name, captured));
                    return Some((rank, params));
                }
            }
        }

        match tail {
            super::PathTail::End => {
                rank.push(MatchRankSeg::ExactEnd);
                Some((rank, params))
            }
            super::PathTail::More(_) => None,
        }
    }

    fn linear_oracle_resolve<'r, 'p, A: Allocator + Copy>(
        routes: &'r [OracleRoute<A>],
        method: Method,
        path: &'p str,
        alloc: A,
    ) -> RouteOutcome<'r, 'p, usize, A> {
        let mut best_rank: Option<OwnVec<MatchRankSeg, A>> = None;
        let mut best_allowed = MethodSet::EMPTY;
        let mut best_match: Option<(&'r usize, OwnVec<(&'r str, &'p str), A>)> = None;

        for route in routes {
            let Some((rank, params)) = linear_match(route.pattern.as_str(), path, alloc) else {
                continue;
            };
            let cmp = match &best_rank {
                None => Ordering::Greater,
                Some(cur) => compare_ranks(rank.as_slice(), cur.as_slice()),
            };
            match cmp {
                Ordering::Greater => {
                    best_rank = Some(rank);
                    best_allowed = MethodSet::single(route.method);
                    best_match = if route.method == method {
                        Some((&route.handler, params))
                    } else {
                        None
                    };
                }
                Ordering::Equal => {
                    best_allowed.insert(route.method);
                    if route.method == method {
                        best_match = Some((&route.handler, params));
                    }
                }
                Ordering::Less => {}
            }
        }

        if let Some((handler, params)) = best_match {
            RouteOutcome::Matched { handler, params }
        } else if !best_allowed.is_empty() {
            RouteOutcome::MethodNotAllowed { allowed: best_allowed }
        } else {
            RouteOutcome::NotFound
        }
    }

    fn draw_pattern<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const STATICS: &[&str] = &["a", "b", "c", "users", "me", "files"];
        let seg_count = tape.int_in(0usize..=4, "pat_segs");
        if seg_count == 0 {
            return OwnString::from_str_in("/", alloc);
        }
        let mut out = OwnString::new_in(alloc);
        for i in 0..seg_count {
            out.push('/');
            let kind = if i + 1 == seg_count {
                tape.int_in(0u8..=2, "seg_kind_last")
            } else {
                tape.int_in(0u8..=1, "seg_kind")
            };
            match kind {
                0 => {
                    if let Some(&s) = tape.pick(STATICS, "static_seg") {
                        out.push_str(s);
                    }
                }
                1 => {
                    // Use depth-indexed canonical param names to keep random route tables rich.
                    let name = format_in!(alloc, ":p{i}");
                    out.push_str(name.as_str());
                }
                _ => {
                    let name = format_in!(alloc, "*w{i}");
                    out.push_str(name.as_str());
                }
            }
        }
        out
    }

    fn draw_path<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const PARTS: &[&str] = &["a", "b", "c", "users", "me", "files", "42", "extra"];
        let count = tape.int_in(0usize..=5, "path_segs");
        let mut out = OwnString::new_in(alloc);
        out.push('/');
        for i in 0..count {
            if i > 0 {
                out.push('/');
            }
            if let Some(&p) = tape.pick(PARTS, "path_part") {
                out.push_str(p);
            }
        }
        if count > 0 && tape.int_in(0u8..=4, "trailing_slash") == 0 {
            out.push('/');
        }
        if tape.int_in(0u8..=3, "add_query") == 0 {
            out.push_str("?k=v#frag");
        }
        out
    }

    fn check_router_against_oracle<A: Allocator + Copy>(
        tape: &Tape,
        plant_swap_static_param: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        const METHODS: &[Method] = &[Method::Get, Method::Post, Method::Put, Method::Delete];
        let route_n = tape.int_in(1usize..=12, "route_count");
        let mut router: Router<usize, A> = Router::new_in(alloc);
        let mut oracle_routes: OwnVec<OracleRoute<A>, A> = OwnVec::new_in(alloc);

        for id in 0..route_n {
            let method = match tape.pick(METHODS, "route_method") {
                Some(&m) => m,
                None => Method::Get,
            };
            let pat = draw_pattern(tape, alloc);
            if router.insert(method, pat.as_str(), id).is_ok() {
                oracle_routes.push(OracleRoute { method, pattern: pat, handler: id });
            }
        }

        let query_n = tape.int_in(1usize..=16, "query_count");
        for _ in 0..query_n {
            let method = match tape.pick(METHODS, "req_method") {
                Some(&m) => m,
                None => Method::Get,
            };
            let path = draw_path(tape, alloc);
            let actual = router
                .resolve_planted(method, path.as_str(), alloc, plant_swap_static_param)
                .map_err(|e| {
                    Broke::Oracle(format_in!(alloc, "resolve alloc failed: {e}").as_str().into())
                })?;
            let expected =
                linear_oracle_resolve(oracle_routes.as_slice(), method, path.as_str(), alloc);
            if actual != expected {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "router mismatch for {method} {}: got {actual:?}, expected {expected:?}",
                        path.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
        }
        Ok(())
    }

    /// The invariant: iterative trie routing matches the linear brute-force static-over-param-over-wildcard precedence oracle on every route table and request path.
    #[test]
    fn property_router_matches_linear_oracle() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            check_router_against_oracle(tape, false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_wrong_precedence() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            check_router_against_oracle(tape, plant, h.alloc())
        });
    }
}
