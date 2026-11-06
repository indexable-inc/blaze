# Flecs script, v4.2.0

Full manual: `docs/FlecsScript.md` at upstream tag v4.2.0 (the vendored dir has only `flecs.{h,c}`; flecs.dev
serves master). **4.2** marks what only v4.2.0 (harness) accepts: the site's build and type checker reject it.
Unmarked features ran on all three builds (see SKILL.md, "Which Flecs runs where"). The site runs each script once,
unmanaged, so the reactivity and async sections never apply there, even where the syntax parses.

## Entities, components, kinds

```
struct Pos(x: f32, y: f32)            // a component type (flecs.meta; `using flecs.meta` optional)
Tag {}                                // an entity used as a tag
prefab Ship { Pos: {1, 2}; Tag }      // prefab; `;` separates statements on one line
ship : Ship {                         // IsA Ship
  Pos: {x: 10}                        // full assign: unnamed members reset (y = 0)
  Pos = {y: 5}                        // **4.2** partial assign: needs Pos already, keeps x
  (Likes, Pizza)                      // pair
  (Start, Pos): {0, 0}                // pair with a value
  cockpit {}                          // child: ship.cockpit
}
Pos p2(x: 1)                          // kind syntax: a component kind always gets a value
"name with spaces" {}; "e_$i" {}; _ : Ship {}; { }   // string, interpolated, anonymous names
$ { TimeOfDay: {0.5} }                // singletons
with Tag, Color(38, 25, 13) { a {}; b {} }           // add to every entity in the block
(IsA, Thing) { (IsA, Animal) { Human {} } }          // a pair scope makes a non-ChildOf hierarchy
```

Types: `struct P3 : P(z: f32)` (inherits members), `struct Pts(v: {f32, count: 3})` (any `Member` field),
`enum Color(Red, Green: 5, Blue)`, `enum E(A, B, {underlying_type: u64})`, `bitmask Top(Bacon, Lettuce)` (u32),
`map Lookup(key_type: i32, type: f32)` then `Lookup: [10: 1, 20: 2]`.

## Expressions

- Operators: `! * / % + - << >> > >= < <= == != & | && ||`. No ternary (`a ? b : c` fails); use `match`.
- `match x { 1: 10; 2: 20; _: 0 }`: an unmatched input without `_` fails the script. Works on bools.
- Component values: `Game[Level].width`; has: `e?[Tag]`, `e?[(Likes, Pizza)]`; methods `e.name()`, `e.path()`,
  `e.parent()`, `e.has(id)`, `e.lookup("a.b")`; `flecs.script.core.pair(a, b)` (bare `pair` needs
  `using flecs.script.core`).
- `new { Pos: {1, 2} }` creates one entity inside an expression (respects scope and `with`).
- Initializers `{10, 20}`, `{x: 10}` need a typed lvalue (`const p: Pos = {1, 2}`). `[1, 2]` alone is a vector
  literal (`vector<i64>`), `[1 .. 5]` materialises a range (end exclusive), `.count()` gives the length.
  Member shorthand: `{x: $, y: $}` takes variables named `x` and `y`.
- Vector types (structs whose members share one primitive type) work element-wise: `p0 + 1`; swizzles: `p.zyx`.
- Strings: `"..."` and backtick `` `...` `` (multiline). Both interpolate `$var` and `{expr}`; escape as `\$`, `\{`.
  Float formats: `"{x:.2}"` (12.35), `"{x:08.1}"` (000012.3), `{x:[[fill]align][+][0][width][.precision][e|E]}`;
  width and precision may be variables or `(expr)`.
- Math (`sqrt`, `lerp`, `PI`, `Rng`...) needs `FLECS_SCRIPT_MATH`; neither blaze build has it.

## Variables, functions, header

```
const pi = 3.14                       // `$pi` when a name clashes with an entity
const wood: Color = {38, 25, 13}
export const tile = 4                 // visible to other scripts and C (scope.tile); never changes
export mut speed = 10                 // may change: C ecs_mut_var_get + ecs_mut_var_modified, async assigns
fn add(a: i32, b: i32) -> i32 { a + b }    // body: const + one expression; no if/for, use match
```

Header statements, in this order, before anything else: `module game` (at most one), `include widgets/button`
(relative to this file, `.flecs` appended, no `..` or absolute paths; managed scripts get their own script entity),
`using game.engines` (or `game.*`).

## Control flow

```
if x > 1 { ... } else if x == 0 { ... } else { ... }
for i in 0..n { "e_{i}" {} }          // also [0..n]; unnamed entities in a loop are new each pass
for (i, v) in [5..10] { }             // index, value
for v in vec { }  for (i, v) in vec { }
for (k, v) in m { }  for (k, i, v) in m { }        // maps
continue                              // `while` exists only in async
```

## Templates

```
template Square {
  prop size: f32 = 10                 // input, set per instance
  mut hover = false                   // reactive state, not an input; readable as e[Square.mut].hover
  Box: {size, size}
  if this?[Damaged] { Tint: {0.5} }   // this = the instance
  inner { Box: {size / 2, 1} }        // child; Parent storage on 4.2, ChildOf on the site
}
Square a(size: 4)                     // or `a { Square: {size: 4} }`, or `Square` in a scope
Sq { template { ... } }               // **4.2**: template named after the entity (inherits its base)
```

- Inheritance: `template Big : Shape { prop size: f32 = 10 }` runs Shape's body first with Big's prop values, then
  Big's (so Big wins on components). Muts and async blocks are inherited; a mut cannot be redeclared.
- Interfaces: a `struct Facade(height: f32)` plus `template Victorian : Facade {...}` lets a prop take any matching
  template: `prop facade: template Facade`, instantiated as `facade: {3}`. A list: `prop fs: template Facade[] = [A, B]`
  (`fs[i]`, `fs.count()`, `for f in fs`). The element type must be the interface struct, not a template.
- Parent constraints: `template Item : parent Group {...}` must be a direct child of a Group instance; it reads the
  parent's props and muts as variables (`parent.x` when masked), and may assign the parent's muts from async.
- From C: set a struct with the template's name, namespace and members (and `Template.mut` for muts).

## Reactivity (managed scripts: `ecs_script(world, {...})`)

- Fine-grained: when an input changes only the statements that read it rerun. Entities keep their ids. Inputs:
  component values and presence (`game[T]`, `game?[T]`), `export mut` variables, props, muts, `$var[T]`. A reference
  to a missing entity or component waits for it.
- Components arrive via `OnSet`: after writing through `ensure`/`get_mut`, or from a system, call `modified()`.
- Each component on an entity is owned by one scope: a full assignment in two scopes is an error unless they are
  branches of one if/else chain. Other scopes may only assign partially (`=`).
- Conditional entities and components are deleted or removed when their condition turns false. Anonymous entities
  in a loop are recreated on each update; name them (`"e_{i}"`) to keep them.

## Async (**4.2**, `FLECS_SCRIPT_ASYNC`)

Not compiled in anywhere in blaze yet: harness's `hyperion-flecs-c` (`crates/harness/vendor/hyperion-flecs-c/build.rs`)
has `FLECS_SCRIPT` but not `FLECS_SCRIPT_ASYNC` or `FLECS_SCRIPT_INPUT`. Add them there before writing `async`.

```
using flecs.script
template Button {
  mut pressed = false
  async {                             // root of a managed script, a template body, or a template child
    while true {
      const e = await on.press(this)  // flecs.script.input (FLECS_SCRIPT_INPUT)
      pressed = true
      await on.release(this)
      pressed = false
    }
  }
}
```

- Async blocks only assign muts (`x = v`); they cannot create entities or set components, cannot nest, and cannot sit
  in an `if` or a plain entity scope. They may use `const`, `if`, `for`, `while`, `try { } catch(Err) { } catch { }`.
- They start and resume in `ecs_progress` (or `ecs_script_tasks_progress`), stop at the end or when the script or
  instance goes, and restart from the top when a prop changes (a parent mut change does not restart them).
- C async functions: `ecs_async_function` with a callback that gets an `ecs_script_future_t`; complete it with
  `ecs_script_future_resolve` / `_reject` / `_reject_id` (any thread), then `ecs_script_future_release`.
- Input events (`on.`): `press release click drag move enter leave` (MouseEvent), `key_down key_up` (KeyboardEvent:
  `key` such as `"Enter"`), `focus blur`, `activate` (click, Enter, or Space; `keyboard` is false for a click).
  There is no double-click event. Events bubble to parents until `await e.stop_propagation()`. The app feeds them with
  `ecs_script_mouse_event`/`ecs_script_keyboard_event`; `await flecs.script.focus(e)` moves focus.

## Hierarchy storage and the Copy rename

- `@tree ChildOf` before a `prefab`/`template` keeps named `ChildOf` children; `@tree Parent` opts a plain scope into
  `Parent` storage. On 4.2, prefab and template children default to `Parent`.
- `(OnInstantiate, Copy)` is **4.2**; the site still writes `(OnInstantiate, Override)`, which 4.2 rejects.

## Traps

- `{` and `$` start interpolation in every string, backticks included. `\{` and `\$` escape them; `\}` is an invalid
  escape (a bare `}` is fine); write `` \` `` for a backtick inside a backtick string.
- Reserved words cannot be entity names: `async await catch const continue else export fn for if in include match
  module mut new prop script template try using while with` (`Section try(...)` fails with "missing ; or newline").
  Quote the name (`"try"`). `prefab`, `struct`, `enum`, `parent` and `this` do work as names.
- An entity used in an expression (`x?[Tag]`, `x[C]`) must exist before the script runs: a plain `Tag {}` earlier in
  the same file is "unresolved reference". Types declared with `struct`/`enum`/`map` in the same file are fine.
- Several statements on one line need `;` (`with Tag { a {}; b {} }`), components too: `x : P { A: {v: 1}; B: {v: 2} }`
  (without the `;`, "missing ; or newline"). One component a line reads best.
- A component written in a scope without a value (`Pos`) default-constructs it.
- A script that fails leaves the entities it already made; templates' resources stay until their instances go.

## Trying syntax

To check a claim, run it through harness's 4.2 build: a C file that calls `ecs_script(world, { .filename = argv[1] })`
and prints `EcsScript.error`, built with `cc -I$F t.c $F/flecs.c`, where `$F` is the `distr` directory of a checkout of
indexable-inc/flecs at the rev in `crates/harness/vendor/hyperion-flecs-c/Cargo.toml` (seconds on the Mac). For the site, compile against flecs-opinionated's `vendor/flecs/distr` with
`-DFLECS_CUSTOM_BUILD -DFLECS_SCRIPT -DFLECS_PREFAB -DFLECS_LOG`, or run `site/flecs/flecs-doc.wasm`.
