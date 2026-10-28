---
name: flecs
description: Flecs (v4.1.6) as blaze's site uses it: every page is a Flecs script (site/content/*.flecs) built from prefabs and element types in schema.flecs, run at build time by Flecs compiled to wasm (site/flecs/flecs-doc.wasm) and rendered to HTML by site/gen. Covers the script syntax and its traps (string escapes, keywords, names), the schema's conventions, relationship traits, and how to rebuild the wasm. Load before editing any .flecs file, site/gen/tree.ts or site/flecs/.
---

# Flecs in blaze

Adapted from den's flecs skill (`den-flecs`, same Flecs version); den's Rust API and app conventions are left out because blaze uses Flecs only through script. Script reference: `script.md`.

## Where Flecs runs

- **Pages are scripts.** `site/content/schema.flecs` defines the element types (structs such as `P`, `Li`, `Code`, `Section`, `Issue`; container tags `Ul`, `Ol`, `Table`, `Tr`; widget tags `RfcIndex`, `References`, `Course`, `UpdateLog`, `BenchHistory`), the relationships (`Status`, `Cites`, `DependsOn`, and the course graph's `Requires` and `InTheme`) and the page prefabs (`Page`, `Doc : Page`, `Rfc : Doc`). `refs.flecs` holds `Ref` entities. Every other file adds pages under `site { ... }`; entity paths are URLs (`site.rfcs.capabilities` is `/rfcs/capabilities`, `site.index` is `/`).
- **Order:** `schema.flecs`, `refs.flecs`, then the rest in path order, all in one world (`site/gen/build.ts`). Names reopen across files, so `site { rfcs { x : Rfc {...} } }` in its own file adds a child to the existing `rfcs`.
- **The build runtime** is `site-snapshot.wasm` (`site/renderer/snapshot`): Flecs's C compiled in through flecs-opinionated's `flecs-c` (wasm32-unknown-unknown, no WASI), pinned by commit in `site/renderer/Cargo.toml`. `gen/build.ts` runs the scripts in it and gets a DAG-CBOR snapshot (`.gen/site.cbor`) of every entity under the roots. Rust modules are built on a dev host or in CI and fetched with `renderer/fetch.py`; see `site/renderer/README.md`.
- **The renderer** is `site.wasm` (`site/renderer/core`, Rust, no Flecs inside): `world.rs` is the entity store loaded from the snapshot (prefab values merged, nearest wins), `render.rs` maps each element type to HTML and builds the widgets, `spoken.rs` derives the narration. The build renders static pages with it, the worker renders comment pages per request with the same module. A node with no known element type fails the build with its path.
- **The type checker** (`site/flecs-mapper`, TS 7.1 content mapper) still runs scripts in the older `site/flecs/flecs-doc.wasm` (`flecs-doc.c` plus amalgamated Flecs, wasm32-wasi, built by `site/flecs/build.py`).

## Rules (general Flecs, carried over from den)

- **Component-wise design.** One small component per concern, a tag for a bit of state, a pair for a link. A node is the set of components it carries; the renderer dispatches on them.
- **Prefabs for shared shape.** `Page`, `Doc`, `Rfc` carry what every instance shares (`OrderedChildren`, `Discussable`, `(Status, Draft)`). An instance overrides by setting its own (`(Status, Discussing)` replaces Draft: `Status` is Exclusive). Default `OnInstantiate` is `Override`: the instance gets a copy.
- **Relationships, not strings, for links.** `(Cites, refs.koka)`, `(DependsOn, site.charter)`. An unknown target fails the script at once, so a broken link cannot ship. Inline `[[name]]` references are checked by the renderer instead.
- **Traits before first use.** Declare a trait right after defining the type, in schema.flecs, before any entity uses it (`Status { Exclusive }`, `Section { (With, flecs.core.OrderedChildren) }`). Symmetric, PairIsTag and Final abort if added late.
- **Child order is `OrderedChildren`.** Without it `ecs_children` returns table order, not script order. Every container type gets it through `(With, OrderedChildren)`; every page through the `Page` prefab. A new container type needs the same line.
- **Names are anchors.** In a `Doc` (anything people comment on), every block needs a name: its path under the page is the comment anchor (`motivation.why`). Renaming or moving a block orphans its comments (they still show, under the page discussion, with their quote). Choose names that describe the content, not its position.
- **Never persist a raw entity id.** Ids differ between runs; persist paths.

## The trait cheat sheet (details: https://www.flecs.dev/flecs/md_docs_2ComponentTraits.html)

| Trait | Say it when | Gotcha |
|---|---|---|
| `Exclusive` | a relationship holds one target per entity (one-of state, owner, parent) | add replaces; fires OnRemove for the old pair then OnAdd for the new |
| `Symmetric` | `R(a,b)` implies `R(b,a)` (peer, adjacent, collaborating) | declare before any use (aborts); needs both ends alive |
| `Transitive` | `R(a,b), R(b,c)` implies `R(a,c)` in queries (LocatedIn) | query-time only; implies Traversable and Acyclic |
| `Reflexive` | `R(x,x)` is true for every x (IsA-like) | only changes query answers |
| `Acyclic` / `Traversable` | the relation is a DAG and queries traverse it with `up`/`cascade` | cycles are not reliably detected |
| `OneOf` | the target must be a child of an enum entity (`(Mode, Dark)`) | violation aborts at add |
| `With` | adding A must also add B | the only trait allowed after a query exists |
| `Relationship` / `Target` / `Trait` | entity may only be used as relationship / target / trait | misuse panics |
| `PairIsTag` | a pair never carries data, even when the target is a component | declare before use |
| `Final` | no entity may `IsA` this component | aborts if already inherited from |
| `Inheritable` | the component may be an `IsA` target and queries honour it | opt-in; tags inherit anyway |
| `OnInstantiate` `Override` / `Inherit` / `DontInherit` | how a prefab's component reaches instances (copy / share / skip) | default `Override` copies |
| `Sparse` | stable pointers, cheap add/remove, no row move | slower iteration |
| `DontFragment` | very sparse pair or component, avoids a table per target | no `target_for`, no Or/Optional/AndFrom/NotFrom, no inheritance, no monitors |
| `CanToggle` | enable/disable without a table move, value kept | small query overhead for all |
| `Singleton` | one value per world, stored on the component entity | cannot be added to other entities |
| `OrderedChildren` (on a parent) | children keep creation or custom order | `ecs_set_child_order` aborts on a wrong set |
| `OnDelete` / `OnDeleteTarget` + `Remove` / `Delete` / `Panic` | what happens to users of an entity that is deleted | default Remove (tags, pairs), Panic (components); `ChildOf` is `OnDeleteTarget Delete` |

Built-in implications: Traversable implies Acyclic; Transitive implies Traversable; DontFragment implies Sparse; Module implies Singleton. `ChildOf`, `OnDelete`, `OnDeleteTarget`, `OnInstantiate` are Exclusive. `IsA`, `ChildOf`, `SlotOf`, `DependsOn`, `With` are PairIsTag.


## Order and sorting

- **Script has no sort.** Flecs script declares entities; it cannot order or query them. Sorting belongs to whoever reads the world.
- **Child order is creation order, nothing more.** `OrderedChildren` keeps the order entities were created in. Across files that order depends on file names (scripts run in path order) and on forward references: naming `site.rfcs.x` in a pair, or opening `x {}` early to make a link resolve, creates `x` at that point. RFC 0003 opening `"linear-types" {}` is how the RFC index once read 0001, 0003, 0002.
- **Use child order only for things whose order the author writes in one place:** the blocks of one page, the items of one list. Never for things collected across files.
- **Sort by data for everything else.** Put the key in a component (`Number` on an `Rfc`, `order` on `Nav` and `Link`) and sort on it, with the path as tie-breaker so ties are stable. In the renderer this is `World::rfcs(parent)` (by `Number`) and the nav (by `order`). In live Flecs it is a query with `order_by` (sorts results by a component with a compare callback; cached queries keep the sort) or `group_by` (buckets results by a key, such as a relationship target); see the voice repo's flecs skill `rust-api.md` for the Rust form.
- **Narration and the page must use the same order.** `spoken.rs` reads lists with the same helpers as `render.rs`, so the spoken RFC index matches the visible one.

## Gotchas found building the site

- Strings interpolate: `{` and `$` start expressions even in backtick strings. Write `\{` and `\$`; `\}` is an invalid escape, a bare `}` is fine. Inside a backtick string write `` \` `` for a backtick.
- `try`, `for`, `if`, `in`, `with`, `using`, `module`, `const`, `prop`, `match`, `new`, `export`, `template`, `prefab`, `struct`, `enum` and other keywords cannot be entity names (`Section try(...)` fails with "missing ; or newline"). Quote the name (`"try"`) or pick another.
- A prefab body cannot name an entity defined later (`(DependsOn, site.charter)` in `Rfc` fails in schema.flecs, which runs first). Put such pairs on instances.
- Kind syntax `P why("...")` is `why { P: {"..."} }`: positional initializer, and the scope is optional. `Tag name` works for tags (`RfcIndex list`).
- Several statements on one line need `;` (`Tr head { Th a("x"); Th b("y") }`).
- Script errors print `file: line: message` with the source line; flecs-doc exits non-zero and the build stops.
