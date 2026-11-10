---
name: world
description: The harness app's own flecs world from ts (world.*): read and change its theme tokens, query entities, get and set components, run flecs script, and manage the edits kept across launches; read before changing how the app looks or behaves.
---
The harness app is drawn from a flecs world: every view is an entity made from a prefab, every colour, size, radius
and spring is a component or a scene token. `world.*` in a ts cell reads and edits that world while the app runs.
Use it when he asks to change the app itself ("make the accent orange", "bigger chat text", "what colour is the
sidebar", "make the theme closer to my Ghostty config"): an edit here shows at once and needs no build. Never
edit `00-tokens.flecs` or any scene file in a checkout to restyle the running app: that is source nobody is
running. A change to what the app can do is still code.

A whole theme is a handful of tokens. Read his colours (Ghostty: `~/.config/ghostty/config` and the theme file it
names; `background`, `foreground`, `palette = N=#hex`), then set the grounds (`tone_ground`, `tone_sidebar`,
`tone_card`, `tone_raised`, `tone_well`), the inks (`tone_ink_strong`, `tone_ink`, `tone_ink_muted`,
`tone_ink_faint`), the lines (`tone_line`, `tone_line_strong`) and `tone_accent`, the `dark` member only for a dark
theme, a few at a time, looking at the app between. `world.query('Fill, (ChildOf, hn.Theme), Prefab')` lists every
role with its colour now.

Names: an entity is its path with dots (`hn.Theme.card`, `hn.ChatPane`); a component is its name (`Fill`, `Size`,
`TextStyle`, `Shape`, `Padding`); a token is a variable of scene/00-tokens.flecs (`tone_accent`, `size_body`).
Values are JSON as flecs writes them; a colour is `{dark: {r, g, b, a}, light: {r, g, b, a}}`, each 0 to 1.

- `await world.token('tone_accent')`: a token's value. `await world.token('tone_accent', {dark: {r: 0.95, g: 0.55,
  b: 0.2}})` writes the members named (the rest stay) and returns the value after; every prefab and view made from
  the token follows. Only `export mut` tokens change while the app runs: every colour (`tone_*`) is one; sizes,
  radii and springs are `export const`, so change those on the prefab that uses them with `set`.
- `await world.query('Fill, (ChildOf, hn.Theme), Prefab')`: a Table of `{entity, values}`, the entities a flecs
  query matches with each term's value in order. Prefabs (the theme's roles, every `hn.*` and `ui.*` view) only
  match with a `Prefab` term; without it a query sees the live instances.
- `await world.get('hn.Theme.card')`: one entity whole: `{parent, name, tags, pairs, components: {Fill: {...}}}`,
  what it inherits from its prefab too.
- `await world.set('hn.ChatBubble', 'Shape', {radius: 14})`: writes the members named into that component of that
  entity and returns the component after. Set it on a prefab to change everything made from it later; views already
  on screen keep their own copy unless they inherit it, so for a colour prefer the token.
- `await world.script('prefab hn.Mine : ui.Row { ... }')`: runs flecs script (the scene's own language; the `flecs`
  skill has it) in the app's world.
- `await world.schema('Fill')`: a component's type; `world.schema()` every one.

Saved edits: `token` and `set` are kept across launches (pass `{save: false}` to try one without keeping it);
`script` only with `{save: true}`. `await world.saved()` lists them in order; `await world.forget(i)` drops one and
`world.forget()` all of them; what a dropped edit did stays until the app is launched again. They are a journal at
`~/.harness/world.json`, replayed after the scene loads.

Refusals name what and why: no such entity, component or token; a value that does not fit its type; a component the
app writes itself every frame (a write would be lost). The app must be running; a headless agent with no app gets
an error saying so.

Read before you write: `world.token(name)` or `world.get(path)` first, change the fewest members, and say in one
line what changed. To find what draws something, query for it (`world.query('Text, Laid')`) or read the scene
(crates/harness/harness-native/scene/).
