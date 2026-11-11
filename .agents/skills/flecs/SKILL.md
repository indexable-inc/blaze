---
name: flecs
description: "Flecs reference, current to v4.2.0: which Flecs each part of blaze runs (harness v4.2.0; the site's runtime, labelled v4.1.6, is a pre-4.2 master build), design rules, the trait cheat sheet, hierarchies (ChildOf vs 4.2's Parent storage), the 4.2 C API changes (explicit deferral, Copy), and the full Flecs script language in script.md with what needs 4.2. Then the site: every page is a Flecs script (site/content/*.flecs) run at build time and rendered to HTML. Load before writing or reviewing any .flecs file, Flecs C/Rust binding code, site/gen/tree.ts or site/flecs/."
---

# Flecs

General Flecs first, then the site. Building native UI out of Flecs (the harness app, `crates/harness`) is the
`flecs-ui` skill; it uses the language described here. Script language reference: `script.md`.

Sources, in order of trust: harness's v4.2.0 `distr/flecs.{h,c}` (the indexable-inc/flecs fork at the rev in `crates/harness/vendor/hyperion-flecs-c/Cargo.toml`; after a build, cargo's checkout of it is under `~/.cargo/git/checkouts/flecs-*`), the upstream v4.2.0 docs
(`docs/FlecsScript.md`, `ComponentTraits.md`, `HierarchiesManual.md`, `Systems.md` at tag v4.2.0; flecs.dev serves
master), then blog posts never. The "needs 4.2" marks here and in `script.md` were checked on 2026-11-03 by running
small scripts through each build listed below.

## Which Flecs runs where

| Runtime | Flecs | Pinned in |
| --- | --- | --- |
| harness native app | v4.2.0 (fork `indexable-inc/flecs`, branch `indexable/v4.2`) | `flecs-distr` rev in `crates/harness/vendor/hyperion-flecs-c/Cargo.toml` |
| site build (`site-snapshot.wasm`) | upstream master `785ea7c` (2026-10-15, 90 commits before v4.2.0); its header says 4.1.6. Addons: SCRIPT, PREFAB, LOG only | flecs-opinionated rev in `site/renderer/Cargo.toml` |
| site type checker (`flecs-doc.wasm`) | `v4.1.6-665-gce3b06a6e`; accepted and rejected the same scripts as `785ea7c` | `site/flecs/FLECS_VERSION` |

The site is not on the shared pin: its runtime is flecs-opinionated's `flecs-c`, whose own vendored flecs is still
pre-4.2 (main at upstream `37a7ef90d` on 2026-11-04). Moving it means porting flecs-opinionated to 4.2, then the
site's `.flecs` files (`Override` to `Copy` and the rest of `script.md`'s **4.2** marks) and rebuilding `flecs-doc.wasm`.

"v4.1.6" on the site means the header, not the release: the v4.1.6 tag's script language is older (it rejects
`struct P(x: f32)`, typed `prop`, `match`, vector literals, maps, `export mut`). Never write site script from 4.1.6-era
docs. Use the 4.2 language and avoid what `script.md` marks **4.2**.

## Rules

- **Component-wise design.** One small component per concern, a tag for a bit of state, a pair for a link. An entity
  is the set of components it carries, and readers dispatch on them.
- **Prefabs for shared shape.** A prefab carries what every instance shares, and an instance overrides by setting its
  own value. By default `OnInstantiate` copies: on 4.2 it is called `Copy`; before 4.2, and on the site, it was
  `Override`. Exclusive relationships replace, so `(Status, Discussing)` on an instance replaces the prefab's
  `(Status, Draft)`.
- **Split a value where instances differ.** A component written in script sets every member (members left out are 0),
  so one component mixing per-instance members with shared ones makes every instance restate all of them. Give each
  concern its own component so the prefab holds the defaults and an instance writes only what differs (harness-video's
  sounds: `Sound {voice}`, `Onset {at}`, `Lasts`, `Pitch`, `Gain`, `Pan` on the `sound.*` prefabs, `src/sound.flecs`;
  `ding : sound.Bell { Onset: {at: 2.0} }` takes the rest from the prefab). Ship shared prefabs once, in a script the
  host loads before every scene, never copied into each scene. Reach for a template only when its body computes from
  its props.
- **Relationships, not strings, for links.** `(DependsOn, x)`: an unknown target fails the script at once.
- **Traits before first use.** Declare a trait right after the type, before any entity uses it. Symmetric,
  PairIsTag and Final abort if added late.
- **Child order needs `OrderedChildren`** on the parent. Without it, `ecs_children` returns table order, not script
  order (queries ignore it either way).
- **Never persist a raw entity id.** Ids differ between runs; persist paths.
- **Script declares; it does not sort or query.** Ordering belongs to whoever reads the world: put the key in a
  component and sort on it (path as tie-breaker), or use a query with `order_by` (cached queries keep the sort) or
  `group_by`. Use child order only for things whose order is written in one place (the blocks of one page).

## Traits (details: `docs/ComponentTraits.md` at v4.2.0)

| Trait | Say it when | Gotcha |
|---|---|---|
| `Exclusive` | a relationship holds one target per entity (one-of state, owner, parent) | add replaces; fires OnRemove for the old pair then OnAdd for the new |
| `Symmetric` | `R(a,b)` implies `R(b,a)` (peer, adjacent) | declare before any use (aborts); needs both ends alive |
| `Transitive` | `R(a,b), R(b,c)` implies `R(a,c)` in queries (LocatedIn) | query-time only; implies Traversable and Acyclic |
| `Reflexive` | `R(x,x)` is true for every x (IsA-like) | only changes query answers |
| `Acyclic` / `Traversable` | the relation is a DAG and queries traverse it with `up`/`cascade` | cycles are not reliably detected |
| `OneOf` | the target must be a child of an enum entity (`(Mode, Dark)`) | violation aborts at add |
| `With` | adding A must also add B | the only trait allowed after a query exists |
| `Relationship` / `Target` / `Trait` | entity may only be used as relationship / target / trait | misuse panics |
| `PairIsTag` | a pair never carries data, even when the target is a component | declare before use |
| `Final` | no entity may `IsA` this component | aborts if already inherited from |
| `Inheritable` | the component may be an `IsA` target and queries honour it | opt-in; tags inherit anyway |
| `OnInstantiate` `Copy` / `Inherit` / `DontInherit` | how a prefab's component reaches instances (copy / share / skip) | default `Copy`; named `Override` before 4.2 (the site) |
| `Sparse` | stable pointers, cheap add/remove, no row move | slower iteration |
| `DontFragment` | very sparse pair or component, avoids a table per target | no `target_for`, no Or/Optional/AndFrom/NotFrom, no inheritance, no monitors |
| `CanToggle` | enable/disable without a table move, value kept | small query overhead for all |
| `Singleton` | one value per world, stored on the component entity | cannot be added to other entities |
| `OrderedChildren` (on a parent) | children keep creation or custom order | `ecs_set_child_order` aborts on a wrong set |
| `OnDelete` / `OnDeleteTarget` + `Remove` / `Delete` / `Panic` | what happens to users of an entity that is deleted | default Remove (tags, pairs), Panic (components); `ChildOf` is `OnDeleteTarget Delete` |

Built-in implications: Traversable implies Acyclic; Transitive implies Traversable; DontFragment implies Sparse;
Module implies Singleton. `ChildOf`, `OnDelete`, `OnDeleteTarget`, `OnInstantiate` are Exclusive. `IsA`, `ChildOf`,
`SlotOf`, `DependsOn`, `With` are PairIsTag. In a 4.2 custom build, Acyclic, Final, OneOf, Relationship, Singleton,
Symmetric, Target and Trait need the `FLECS_CONSTRAINT_TRAITS` addon.

## Hierarchies: ChildOf and Parent storage (4.2)

- `ChildOf` (a pair) puts each parent's children in their own table. `Parent` (a component whose value is the parent,
  plus an automatic `(ParentDepth, @n)` pair) keeps children of many parents in one table. That makes prefab
  instantiation cheap (no new tables, a cached "TreeSpawner"), but deleting one child is O(n) in its siblings, and
  queries that mix a `ChildOf` term with others get slower. See `HierarchiesManual.md`, "Hierarchy storage".
- In script, plain scopes use `ChildOf`. On 4.2, **children of prefabs and templates default to `Parent`**, and
  instance children of a `Parent` prefab come out unnamed (`#493`, not `inst.kid`). Write `@tree ChildOf` before
  the `prefab`/`template` to keep names and `ChildOf`; `@tree Parent` opts a plain scope in. Before 4.2 (the site),
  prefab and template children use `ChildOf`.

## Flecs 4.2 C API changes (for binding and system code)

- **Explicit deferral.** In `ecs_iter_t`, `it->world` is the real world and `it->stage` the deferred one; v4.1.6 had
  `it->world` (maybe a stage) and `it->real_world`. In a normal (readonly) system a structural change through
  `it->world` asserts (`EcsWorldReadonly`); pass `it->stage` to queue it. In an `immediate` system `it->world` applies
  at once and `it->stage` still queues. C++: `it.stage()`, `it.world()`, `e.stage(it.world())`.
- `(OnInstantiate, Override)` is `(OnInstantiate, Copy)`: `EcsOverride` no longer exists.
- `ecs_entity_desc_t` lost `add`, `set` and `add_expr`; give a system its phase with `ecs_system_desc_t.phase`.
- Prefabs, constraint traits, cached queries and query plans are now addons (`FLECS_PREFAB`,
  `FLECS_CONSTRAINT_TRAITS`, `FLECS_CACHED_QUERIES`, `FLECS_QUERY_PLANS`); a `FLECS_CUSTOM_BUILD` must name them.
  New script addons: `FLECS_SCRIPT_ASYNC`, `FLECS_SCRIPT_INPUT` (was `FLECS_SCRIPT_EVENT` before the release),
  `FLECS_SCRIPT_SHELL`, `FLECS_SCRIPT_PLATFORM`, `FLECS_SCRIPT_MATH`.

## The site (runtime labelled v4.1.6; see the table above)

### Where Flecs runs

- **Pages are scripts.** `site/content/schema.flecs` defines the element types (structs such as `P`, `Li`, `Code`,
  `Section`, `Issue`; container tags `Ul`, `Ol`, `Table`, `Tr`; widget tags `RfcIndex`, `References`, `Course`,
  `UpdateLog`, `BenchHistory`), the relationships (`Status`, `Cites`, `DependsOn`, and the course graph's `Requires`
  and `InTheme`) and the page prefabs (`Page`, `Doc : Page`, `Rfc : Doc`). `refs.flecs` holds `Ref` entities. Every
  other file adds pages under `site { ... }`; entity paths are URLs (`site.rfcs.capabilities` is `/rfcs/capabilities`,
  `site.index` is `/`).
- **Order:** `schema.flecs`, `refs.flecs`, then the rest in path order, all in one world (`site/gen/build.ts`). Names
  reopen across files, so `site { rfcs { x : Rfc {...} } }` in its own file adds a child to the existing `rfcs`.
- **The build runtime** is `site-snapshot.wasm` (`site/renderer/snapshot`): Flecs's C compiled in through
  flecs-opinionated's `flecs-c` (wasm32-unknown-unknown, no WASI). `gen/build.ts` runs each script once with
  `ecs_script_run` (unmanaged: no reactivity, no async) and gets a DAG-CBOR snapshot (`.gen/site.cbor`) of every entity
  under the roots. Rust modules are built on a dev host or in CI and fetched with `renderer/fetch.py`; see
  `site/renderer/README.md`.
- **The renderer** is `site.wasm` (`site/renderer/core`, Rust, no Flecs inside): `world.rs` is the entity store loaded
  from the snapshot (prefab values merged, nearest wins), `render.rs` maps each element type to HTML and builds the
  widgets. The build renders static pages with it, the worker renders comment pages per request with the same module.
  A node with no known element type fails the build with its path.
- **The type checker** (`site/flecs-mapper`, TS 7.1 content mapper) runs scripts in `site/flecs/flecs-doc.wasm`
  (`flecs-doc.c` plus amalgamated Flecs, wasm32-wasi, built by `site/flecs/build.py`). A script must pass both.

### Site conventions

- `Page`, `Doc`, `Rfc` carry what every instance shares (`OrderedChildren`, `Discussable`, `(Status, Draft)`).
  Every container type gets `(With, flecs.core.OrderedChildren)`, and a new container type needs the same line.
- Links are pairs: `(Cites, refs.koka)`, `(DependsOn, site.charter)`. Inline `[[name]]` references are checked by the
  renderer instead.
- **Names are anchors.** In a `Doc` (anything people comment on), every block needs a name: its path under the page
  is the comment anchor (`motivation.why`). Renaming or moving a block orphans its comments (they still show, under
  the page discussion, with their quote). Choose names that describe the content, not its position.
- **Sort by data across files.** Child order is creation order, and across files that depends on file names and
  forward references: naming `site.rfcs.x` in a pair, or opening `x {}` early, creates `x` at that point (RFC 0003
  opening `"linear-types" {}` once made the index read 0001, 0003, 0002). Sorted things carry a key (`Number` on an
  `Rfc`, `order` on `Nav` and `Link`); the renderer sorts in `World::rfcs(parent)` and the nav.

### Gotchas found building the site

- A prefab body cannot name an entity defined later (`(DependsOn, site.charter)` in `Rfc` fails in schema.flecs,
  which runs first). Put such pairs on instances.
- Kind syntax `P why("...")` is `why { P: {"..."} }`. `Tag name` works for tags (`RfcIndex list`).
- Script errors print `file: line: message` with the source line; the build stops.
- String escapes, reserved words and the rest of the language traps: `script.md`, "Traps".
