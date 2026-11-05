# flecs-content-mapper

A TypeScript 7.1 content mapper for the site's `.flecs` scripts. With it, `tsc` and the editor report
mistakes in the content at the place they were written: a wrong member or value type in a component, an
unknown component or tag, a `(Cites, refs.x)` or `[[x]]` that names no Ref, a `(DependsOn, site.y)` that
names no page.

The input stays plain Flecs script. Flecs decides what it means: for each file the mapper runs the site's
scripts under the site's own Flecs (`../flecs/flecs-doc.wasm`, the binary the build uses), in the build's
order (`schema.flecs`, `refs.flecs`, then the rest by path), up to and including the file being checked,
with the editor's text. TypeScript only adds the checks Flecs does not make.

It relies on the TypeScript 7.1 beta content mapper protocol (microsoft/typescript-go#4712), which is not
stable yet. Node 23.6 or later runs the TypeScript source directly.

## Enable it

1. Make the package resolvable by name from the tsconfig's directory. `contentMappers[].package` must be a
   bare module name (a relative path fails with TS18075). The site does this with a bun workspace, so
   `site/node_modules/flecs-content-mapper` is a symlink to this directory:

   ```json
   "workspaces": ["flecs-mapper"],
   "devDependencies": { "flecs-content-mapper": "workspace:*", "typescript": "7.1.0-beta" }
   ```

   It must be a link, not a copy: `file:` makes bun copy the package under `node_modules`, and Node
   refuses to strip types from files under `node_modules` (`ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING`),
   so the mapper process dies before it answers. Through the link, Node runs the real path. A link also
   means edits here take effect without reinstalling.

2. Add a tsconfig next to the content, for example `site/content/tsconfig.json`:

   ```json
   {
     "compilerOptions": { "strict": true, "noEmit": true, "types": [], "lib": ["es2024"] },
     "contentMappers": [
       { "package": "flecs-content-mapper", "extensions": [".flecs"], "options": { "scripts": ".", "wasm": "../flecs/flecs-doc.wasm" } }
     ],
     "include": ["**/*.flecs"]
   }
   ```

3. Check from the command line (the last step of `bun run check`). The flag is required because the
   mapper is a program tsc runs:

   ```sh
   tsc -p content --runExternalCode
   ```

4. VS Code: install the TypeScript native preview extension (`TypeScriptTeam.native-preview`), trust the
   workspace, and set:

   ```json
   {
     "js/ts.experimental.useTsgo": true,
     "js/ts.tsdk.path": "./node_modules/typescript",
     "js/ts.contentMappers.enabled": true
   }
   ```

Options (all optional):

| option | default | meaning |
| --- | --- | --- |
| `scripts` | `"."` | directory, or array of directories, holding the scripts the build runs, relative to the tsconfig |
| `first` | `["schema.flecs", "refs.flecs"]` | base names that run first, in this order |
| `relationships` | `{"Cites": "Ref", "DependsOn": "Page"}` | what a relationship's target must have (a tag or component) or be an instance of (a prefab) |
| `refs` | `"refs"` | where `[[x]]` and `[label][x]` look for `x`; `null` turns that check off |
| `wasm` | `../flecs/flecs-doc.wasm` | the Flecs build to run |

## What it checks

Flecs's own errors, at the position Flecs gives (`flecs91002`):

- unknown components, tags, prefabs and entity references, including `(Cites, refs.x)` and
  `(DependsOn, site.y)` when the target does not exist;
- unknown members (`P why(nope: 1)`), too many positional values, values Flecs cannot cast
  (`width: "wide"`).

TypeScript errors, on what Flecs accepts but the site does not mean:

- a value of the wrong type that Flecs converts silently (`P x(text: 1)` is stored as `"1"`);
- a relationship target of the wrong kind (`(Cites, site.page)`, `(DependsOn, refs.x)`);
- `[[x]]` and `[label][x]` in a string, read from the text Flecs stored, when `refs.x` does not exist.

Each file declares, as global TypeScript, the entities, struct components and prefabs it defines, so other
files see them. Definitions come only from what Flecs produced, and the generated text depends only on the
scripts' content, so it changes only when the content does.

Where the mapper's reading of the source and Flecs's result disagree (an entity Flecs made that the mapper
could not place, a value Flecs stored differently), it reports that (`flecs91003`) rather than guess.
Codes: 91001 syntax the mapper cannot follow, 91002 Flecs error, 91003 disagreement, 91004 not checked,
91010 bad option.

## What it does not cover

- Statements it does not model: `with`, `template`, `prop`, `const`, `module`, `if`/`for`, annotations,
  pair values and pair scopes, enum and bitmask values. Flecs still runs them; entities they create are
  reported as 91003 ("did not find where"), and their values are not checked.
- Struct inheritance between components.
- Entities whose names are not TypeScript identifiers (`"404"`, `"linear-types"`) are checked by Flecs but
  not declared to TypeScript, so a relationship rule on such a target is reported as 91004.
- Flecs stops at its first error in a file, so a file shows one Flecs error at a time, and the
  TypeScript checks for that file are skipped until it runs.
- A broken script earlier in the build order makes every later file fail with "Flecs failed in <file>".
- References to a script later in the build order fail, as they do in the build.
- `tsc` prints semantic (TS) errors only when no file has a syntactic or mapper error, so on the command
  line a Flecs error in one file hides the TS errors in others. The editor shows both.
- Some TS messages name the generated shapes (`Property '__has_Ref' is missing ...`); the position is
  right, the wording is TypeScript's.
- Nothing is checked for render-time meaning beyond the rules above (for example, that a page has a
  Title).

## Demo

`demo/` has a tsconfig over the real content plus `demo/pages` (correct) and `demo/broken` (one mistake
each):

```sh
cd demo && ../node_modules/.bin/tsc -p . --runExternalCode
```
