//! Flecs 4.2 Script-style declarative ECS scene macro (`ecs_scene!`).
//!
//! Lets you author entities, prefabs (`prefab Name { ... }`), `IsA` inheritance (`Child : Prefab`),
//! nested `ChildOf` + `OrderedChildren` hierarchies, typed components (`Comp = expr;`,
//! `copy Comp = expr;`), tags (`#Tag;`), Flecs relations (`(Rel, target);`), and singletons
//! (`$Singleton = expr;`) in one clean, `#![no_std]`, zero-unsafe declarative block!
//!
//! # Example
//! ```ignore
//! ecs_scene!(&mut world, {
//!     singleton Gravity = Gravity(9.81);
//!
//!     prefab Planet {
//!         copy Mass = Mass(100);
//!     }
//!
//!     let earth = Earth : Planet {
//!         #Habitable;
//!         Position = Position { x: 10, y: 0 };
//!         Moon {
//!             (Orbits, earth);
//!             Position = Position { x: 12, y: 0 };
//!         }
//!     };
//! })?;
//! ```

#[macro_export]
macro_rules! ecs_scene {
    ($world:expr, { $($body:tt)* }) => {
        '__blaze_ecs_scene: {
            let __w = $world;
            $crate::scene_macro::ecs_scene!(@stmts __w, '__blaze_ecs_scene, None, $($body)*);
            ::core::result::Result::<(), $crate::EcsError>::Ok(())
        }
    };

    // -------------------------------------------------------------------------
    // Statement list terminator
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt,) => {};

    // -------------------------------------------------------------------------
    // Singleton: `singleton Comp = expr;`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, singleton $comp:ty = $val:expr ; $($rest:tt)*) => {
        if let ::core::result::Result::Err(__e) = $w.set_singleton::<$comp>($val) {
            break $lbl ::core::result::Result::Err(__e);
        }
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Prefab declaration: `prefab Name { ... }` (binds `Name` as local `Entity` too!)
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, prefab $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        let __pf_tag = $w.prefab();
        if let ::core::result::Result::Err(__err) = $w.add_tag($name, __pf_tag) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Prefab inheriting parent prefab: `prefab Name : Base { ... }`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, prefab $name:ident : $base:expr, { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        let __pf_tag = $w.prefab();
        if let ::core::result::Result::Err(__err) = $w.add_tag($name, __pf_tag) {
            break $lbl ::core::result::Result::Err(__err);
        }
        let __isa = $w.is_a();
        if let ::core::result::Result::Err(__err) = $w.add_pair($name, __isa, $base) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Bound entity with `IsA` prefab: `let var = Name : prefab_expr { ... }`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, let $var:ident = $name:ident : $pf:ident { $($inner:tt)* } $($rest:tt)*) => {
        let $var: $crate::Entity = match $w.instantiate($pf) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        if let ::core::result::Result::Err(__err) = $w.set_name($var, ::core::stringify!($name)) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $var);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $var, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Bound entity: `let var = Name { ... }`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, let $var:ident = $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        let $var: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $var);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $var, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Named entity with `IsA` prefab: `Name : Prefab { ... }`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, $name:ident : $pf:ident { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.instantiate($pf) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        if let ::core::result::Result::Err(__err) = $w.set_name($name, ::core::stringify!($name)) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Named entity: `Name { ... }`
    // -------------------------------------------------------------------------
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, $parent, $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // Optional semicolon between declarations
    (@stmts $w:ident, $lbl:lifetime, $parent:tt, ; $($rest:tt)*) => {
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, $parent, $($rest)*);
    };

    // -------------------------------------------------------------------------
    // Attach `(ChildOf, parent)` & mark parent `OrderedChildren`
    // -------------------------------------------------------------------------
    (@attach_parent $w:ident, $lbl:lifetime, None, $child:ident) => {};
    (@attach_parent $w:ident, $lbl:lifetime, ($parent:ident), $child:ident) => {
        let __ord = $w.ordered_children();
        if let ::core::result::Result::Err(__err) = $w.add_tag($parent, __ord) {
            break $lbl ::core::result::Result::Err(__err);
        }
        if let ::core::result::Result::Err(__err) = $w.relate::<$crate::ChildOf>($child, $parent) {
            break $lbl ::core::result::Result::Err(__err);
        }
    };

    // -------------------------------------------------------------------------
    // Entity body entries: components, `copy` components, `#Tag`, `(Rel, tgt)`,
    // or nested child entities!
    // -------------------------------------------------------------------------
    (@body $w:ident, $lbl:lifetime, $ent:ident,) => {};

    // Typed marker tag: `#Tag;`
    (@body $w:ident, $lbl:lifetime, $ent:ident, # $tag:ty ; $($rest:tt)*) => {
        if let ::core::result::Result::Err(__err) = $w.add_tag_typed::<$tag>($ent) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };

    // Typed relation pair: `(Rel, target);`
    (@body $w:ident, $lbl:lifetime, $ent:ident, ($rel:ty, $tgt:expr) ; $($rest:tt)*) => {
        if let ::core::result::Result::Err(__err) = $w.relate::<$rel>($ent, $tgt) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };

    // Copyable component (propagates through `IsA` / `instantiate`): `copy Comp = expr;`
    (@body $w:ident, $lbl:lifetime, $ent:ident, copy $comp:ty = $val:expr ; $($rest:tt)*) => {
        if let ::core::result::Result::Err(__err) = $w.set_copy::<$comp>($ent, $val) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };

    // Owned or general component: `Comp = expr;`
    (@body $w:ident, $lbl:lifetime, $ent:ident, $comp:ty = $val:expr ; $($rest:tt)*) => {
        if let ::core::result::Result::Err(__err) = $w.set::<$comp>($ent, $val) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };

    // Nested child declarations inside entity body (`prefab`, `let`, `Child : Pf`, `Child { ... }`)
    (@body $w:ident, $lbl:lifetime, $ent:ident, prefab $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        $crate::scene_macro::ecs_scene!(@stmts $w, $lbl, ($ent), prefab $name { $($inner)* });
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
    (@body $w:ident, $lbl:lifetime, $ent:ident, let $var:ident = $name:ident : $pf:ident { $($inner:tt)* } $($rest:tt)*) => {
        let $var: $crate::Entity = match $w.instantiate($pf) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        if let ::core::result::Result::Err(__err) = $w.set_name($var, ::core::stringify!($name)) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, ($ent), $var);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $var, $($inner)*);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
    (@body $w:ident, $lbl:lifetime, $ent:ident, let $var:ident = $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        let $var: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, ($ent), $var);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $var, $($inner)*);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
    (@body $w:ident, $lbl:lifetime, $ent:ident, $name:ident : $pf:ident { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.instantiate($pf) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        if let ::core::result::Result::Err(__err) = $w.set_name($name, ::core::stringify!($name)) {
            break $lbl ::core::result::Result::Err(__err);
        }
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, ($ent), $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
    (@body $w:ident, $lbl:lifetime, $ent:ident, $name:ident { $($inner:tt)* } $($rest:tt)*) => {
        #[allow(non_snake_case, unused_variables)]
        let $name: $crate::Entity = match $w.spawn_named(::core::stringify!($name)) {
            ::core::result::Result::Ok(__e) => __e,
            ::core::result::Result::Err(__err) => break $lbl ::core::result::Result::Err(__err),
        };
        $crate::scene_macro::ecs_scene!(@attach_parent $w, $lbl, ($ent), $name);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $name, $($inner)*);
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
    (@body $w:ident, $lbl:lifetime, $ent:ident, ; $($rest:tt)*) => {
        $crate::scene_macro::ecs_scene!(@body $w, $lbl, $ent, $($rest)*);
    };
}

pub use ecs_scene;
