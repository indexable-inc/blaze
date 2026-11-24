//! Flecs-style declarative typed ECS query macro (`ecs_query!`).
//!
//! Compiles zero-overhead, type-checked, `#![no_std]` Flecs 4.2 query expressions directly onto
//! [`crate::TypedQuery`] and [`crate::QueryFetch`] with full support for:
//! - 1..=4 component fetches (`&T`, `&mut T`) interleaved freely with filter terms
//! - Component/tag filters: `with(T)`, `!T`, `tag(T)`, `!tag(T)` (including Flecs 4.2 `DontFragment` tags!)
//! - Relation pairs & wildcards: `(Rel, target)`, `!(Rel, target)`, `(Rel, *)`, `!(Rel, *)`
//! - Topological parent-before-child hierarchy ordering: `cascade(ChildOf)`
//! - Prefab inclusion toggle: `prefabs`
//! - Four execution styles:
//!   - Builder: `ecs_query!(world, &mut Pos, &Vel, !Frozen, (ChildOf, parent))?`
//!   - Inline `each`: `ecs_query!(world, &mut Pos, &Vel, !Frozen => |pos, vel| { ... })?`
//!   - Inline `entity`: `ecs_query!(world, entity, &Pos, tag(Active) => |e, pos| { ... })?`
//!   - Inline SoA `chunk`: `ecs_query!(world, chunk, &mut Pos, &Vel => |ps, vs| { ... })?`

/// Flecs-style declarative typed query macro over a [`crate::World`].
///
/// # Examples
/// ```ignore
/// // 1. Inline each (1..=4 components + arbitrary Flecs filter terms):
/// ecs_query!(world, &mut Pos, &Vel, !Frozen, tag(Active), (ChildOf, root) => |pos, vel| {
///     pos.x += vel.dx;
/// })?;
///
/// // 2. Inline with Entity handle:
/// ecs_query!(world, entity, &SiteNodeKind, tag(SiteHovered) => |ent, kind| {
///     ...
/// })?;
///
/// // 3. Inline contiguous SoA chunks (SIMD-friendly):
/// ecs_query!(world, chunk, &mut Pos, &Vel, cascade(ChildOf) => |ps, vs| {
///     for (p, v) in ps.iter_mut().zip(vs.iter()) { p.x += v.dx; }
/// })?;
///
/// // 4. Builder mode (chain `.count()`, `.each(...)`, `.build_cached()`):
/// let n = ecs_query!(world, &Pos, (Likes, *), !Dead)?.count()?;
/// ```
#[macro_export]
macro_rules! ecs_query {
    // Entry points: `entity, ...`, `chunk, ...`, or default builder / `each` mode.
    ($world:expr, entity, $($rest:tt)+) => {
        $crate::query_macro::ecs_query!(@parse mode(entity) world($world) fetches() filters() rest($($rest)+))
    };
    ($world:expr, chunk, $($rest:tt)+) => {
        $crate::query_macro::ecs_query!(@parse mode(chunk) world($world) fetches() filters() rest($($rest)+))
    };
    ($world:expr, $($rest:tt)+) => {
        $crate::query_macro::ecs_query!(@parse mode(each) world($world) fetches() filters() rest($($rest)+))
    };

    // -------------------------------------------------------------------------
    // TT-muncher: parse fetches (`&mut T`, `&T`) & Flecs filter terms in any order
    // -------------------------------------------------------------------------

    // Mutable component fetch: `&mut T`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(&mut $ty:ty, $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)* (&mut $ty)) filters($($flt)*) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(&mut $ty:ty => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)* (&mut $ty)) filters($($flt)*) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(&mut $ty:ty $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)* (&mut $ty)) filters($($flt)*))
    };

    // Immutable component fetch: `&T`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(& $ty:ty, $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)* (& $ty)) filters($($flt)*) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(& $ty:ty => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)* (& $ty)) filters($($flt)*) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(& $ty:ty $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)* (& $ty)) filters($($flt)*))
    };

    // Required tag: `tag(T)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(tag($ty:ty), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [with_tag($ty)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(tag($ty:ty) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [with_tag($ty)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(tag($ty:ty) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [with_tag($ty)]))
    };

    // Excluded tag: `!tag(T)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!tag($ty:ty), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [without_tag($ty)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!tag($ty:ty) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [without_tag($ty)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!tag($ty:ty) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [without_tag($ty)]))
    };

    // Required non-fetched component: `with(T)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(with($ty:ty), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [with_comp($ty)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(with($ty:ty) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [with_comp($ty)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(with($ty:ty) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [with_comp($ty)]))
    };

    // Topological hierarchy cascade: `cascade(Rel)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(cascade($rel:ty), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [cascade($rel)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(cascade($rel:ty) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [cascade($rel)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(cascade($rel:ty) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [cascade($rel)]))
    };

    // Include prefabs: `prefabs`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(prefabs, $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [prefabs]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(prefabs => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [prefabs]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(prefabs $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [prefabs]))
    };

    // Wildcard relation pair: `(Rel, *)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, *), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [with_any_rel($rel)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, *) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [with_any_rel($rel)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, *) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [with_any_rel($rel)]))
    };

    // Excluded wildcard relation pair: `!(Rel, *)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, *), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [without_any_rel($rel)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, *) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [without_any_rel($rel)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, *) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [without_any_rel($rel)]))
    };

    // Exact relation pair: `(Rel, target)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, $tgt:expr), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [with_rel($rel, $tgt)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, $tgt:expr) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [with_rel($rel, $tgt)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(($rel:ty, $tgt:expr) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [with_rel($rel, $tgt)]))
    };

    // Excluded exact relation pair: `!(Rel, target)`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, $tgt:expr), $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [without_rel($rel, $tgt)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, $tgt:expr) => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [without_rel($rel, $tgt)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!($rel:ty, $tgt:expr) $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [without_rel($rel, $tgt)]))
    };

    // Excluded component or tag: `!T`
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!$ty:ty, $($rest:tt)+)) => {
        $crate::query_macro::ecs_query!(@parse mode($m) world($w) fetches($($f)*) filters($($flt)* [without($ty)]) rest($($rest)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!$ty:ty => $($body:tt)+)) => {
        $crate::query_macro::ecs_query!(@exec mode($m) world($w) fetches($($f)*) filters($($flt)* [without($ty)]) body($($body)+))
    };
    (@parse mode($m:ident) world($w:expr) fetches($($f:tt)*) filters($($flt:tt)*) rest(!$ty:ty $(,)?)) => {
        $crate::query_macro::ecs_query!(@build world($w) fetches($($f)*) filters($($flt)* [without($ty)]))
    };

    // -------------------------------------------------------------------------
    // Fetch type constructor (1..=4 fetches)
    // -------------------------------------------------------------------------
    (@fetch_ty ($a:ty)) => { $a };
    (@fetch_ty ($a:ty) ($b:ty)) => { ($a, $b) };
    (@fetch_ty ($a:ty) ($b:ty) ($c:ty)) => { ($a, $b, $c) };
    (@fetch_ty ($a:ty) ($b:ty) ($c:ty) ($d:ty)) => { ($a, $b, $c, $d) };

    // -------------------------------------------------------------------------
    // Filter chain emitter (zero closures — breaks labeled block on Err!)
    // -------------------------------------------------------------------------
    (@apply $q:ident, $lbl:lifetime,) => {};
    (@apply $q:ident, $lbl:lifetime, [with_tag($ty:ty)] $($rest:tt)*) => {
        $q = match $q.with_tag::<$ty>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [without_tag($ty:ty)] $($rest:tt)*) => {
        $q = match $q.without_tag::<$ty>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [with_comp($ty:ty)] $($rest:tt)*) => {
        $q = match $q.with_component::<$ty>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [without($ty:ty)] $($rest:tt)*) => {
        $q = match $q.without::<$ty>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [with_any_rel($rel:ty)] $($rest:tt)*) => {
        $q = match $q.with_any_relation::<$rel>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [without_any_rel($rel:ty)] $($rest:tt)*) => {
        $q = match $q.without_any_relation::<$rel>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [with_rel($rel:ty, $tgt:expr)] $($rest:tt)*) => {
        $q = match $q.with_relation::<$rel>($tgt) {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [without_rel($rel:ty, $tgt:expr)] $($rest:tt)*) => {
        $q = match $q.without_relation::<$rel>($tgt) {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [cascade($rel:ty)] $($rest:tt)*) => {
        $q = match $q.cascade::<$rel>() {
            ::core::result::Result::Ok(next) => next,
            ::core::result::Result::Err(err) => break $lbl ::core::result::Result::Err(err),
        };
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };
    (@apply $q:ident, $lbl:lifetime, [prefabs] $($rest:tt)*) => {
        $q = $q.include_prefabs();
        $crate::query_macro::ecs_query!(@apply $q, $lbl, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Builder output: `Result<TypedQuery<'_, FetchTy, A>, EcsError>`
    // -------------------------------------------------------------------------
    (@build world($w:expr) fetches($($f:tt)+) filters($($flt:tt)*)) => {
        '__blaze_ecs_q: {
            let mut __blaze_q = ($w).query::<$crate::query_macro::ecs_query!(@fetch_ty $($f)+)>();
            $crate::query_macro::ecs_query!(@apply __blaze_q, '__blaze_ecs_q, $($flt)*);
            ::core::result::Result::<_, $crate::EcsError>::Ok(__blaze_q)
        }
    };

    // -------------------------------------------------------------------------
    // Inline execution: `each`, `entity`, `chunk` (unpacking 1..=4 arguments cleanly!)
    // -------------------------------------------------------------------------
    // 1 fetch
    (@exec mode(each) world($w:expr) fetches(($a:ty)) filters($($flt:tt)*) body(|$p0:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a)) filters($($flt)*))
            .and_then(|__q| __q.each(|$p0| $body))
    }};
    (@exec mode(chunk) world($w:expr) fetches(($a:ty)) filters($($flt:tt)*) body(|$p0:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a)) filters($($flt)*))
            .and_then(|__q| __q.each_chunk(|$p0| $body))
    }};
    (@exec mode(entity) world($w:expr) fetches(($a:ty)) filters($($flt:tt)*) body(|$e:pat_param, $p0:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a)) filters($($flt)*))
            .and_then(|__q| __q.each_with_entity(|$e, $p0| $body))
    }};

    // 2 fetches
    (@exec mode(each) world($w:expr) fetches(($a:ty) ($b:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b)) filters($($flt)*))
            .and_then(|__q| __q.each(|($p0, $p1)| $body))
    }};
    (@exec mode(chunk) world($w:expr) fetches(($a:ty) ($b:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b)) filters($($flt)*))
            .and_then(|__q| __q.each_chunk(|($p0, $p1)| $body))
    }};
    (@exec mode(entity) world($w:expr) fetches(($a:ty) ($b:ty)) filters($($flt:tt)*) body(|$e:pat_param, $p0:pat_param, $p1:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b)) filters($($flt)*))
            .and_then(|__q| __q.each_with_entity(|$e, ($p0, $p1)| $body))
    }};

    // 3 fetches
    (@exec mode(each) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param, $p2:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c)) filters($($flt)*))
            .and_then(|__q| __q.each(|($p0, $p1, $p2)| $body))
    }};
    (@exec mode(chunk) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param, $p2:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c)) filters($($flt)*))
            .and_then(|__q| __q.each_chunk(|($p0, $p1, $p2)| $body))
    }};
    (@exec mode(entity) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty)) filters($($flt:tt)*) body(|$e:pat_param, $p0:pat_param, $p1:pat_param, $p2:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c)) filters($($flt)*))
            .and_then(|__q| __q.each_with_entity(|$e, ($p0, $p1, $p2)| $body))
    }};

    // 4 fetches
    (@exec mode(each) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty) ($d:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param, $p2:pat_param, $p3:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c) ($d)) filters($($flt)*))
            .and_then(|__q| __q.each(|($p0, $p1, $p2, $p3)| $body))
    }};
    (@exec mode(chunk) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty) ($d:ty)) filters($($flt:tt)*) body(|$p0:pat_param, $p1:pat_param, $p2:pat_param, $p3:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c) ($d)) filters($($flt)*))
            .and_then(|__q| __q.each_chunk(|($p0, $p1, $p2, $p3)| $body))
    }};
    (@exec mode(entity) world($w:expr) fetches(($a:ty) ($b:ty) ($c:ty) ($d:ty)) filters($($flt:tt)*) body(|$e:pat_param, $p0:pat_param, $p1:pat_param, $p2:pat_param, $p3:pat_param| $body:expr)) => {{
        $crate::query_macro::ecs_query!(@build world($w) fetches(($a) ($b) ($c) ($d)) filters($($flt)*))
            .and_then(|__q| __q.each_with_entity(|$e, ($p0, $p1, $p2, $p3)| $body))
    }};
}

pub use ecs_query;
