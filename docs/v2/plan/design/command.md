---
status: plan
summary: The command over the keyed driver, past `check` of a program that only needs std. Build scripts and macro modules as nodes of the one request, what a script prints as the reading unit's own input, an expansion as a fact owned by the macro crate and answered under this request's module, code from plans keyed by what they read and linked by blaze-link from records, and the order, with the code path named as the critical one.
date: 2026-11-14
---
# The command: scripts, macros and code with no daemon

Owner: the main agent. Part of `target.md` (Order, step 2) and `host.md`. Second form, after an
outside review of the first on 2026-11-14 found three ways it served stale results (an
expansion's id held the module's digest; a plan was keyed by type keys alone; a script's cfg was
inside the crate key) and a no-op floor its own phases could not reach. Every number is an
**estimate**. Nothing here is built yet except what "What exists" says.

## What exists (read in the tree, 2026-11-14)

- `blaze-command`: the unit graph from the resolved workspace, the leaf sweep, the request, and
  `Command`, the `Front` over blaze-unitkey and blaze-front: `blaze check --no-daemon` of a
  program that needs only std (std's build scripts are ported, `blaze-units/src/stdlib/scripts.rs`,
  and applied by `graph::std_ran`). Every other script and every proc macro is refused by name
  (`graph::Blocked`). Today it gathers and images every unit's files before the certificate is
  looked up, and it puts a unit's cfg into its crate key.
- `blaze-expansion`: keys, records and a lookup machine for a script run, a macro call and an
  expander module, tested against models. Its machine reads and writes the store itself.
- `blaze-program`: instance keys, the monomorphization walk, a code record and a four-section
  placement, tested against a model world. No backend and no linker behind it.
- The daemon: everything that really runs a script, compiles and runs a macro, makes code and
  links, tied to its `Job`, to a live database and to the old store. blaze-codegen's front end
  works only on a live database of every crate the program reaches.

## One request, one certificate

- A build script's run and a macro crate's module are **nodes of the request's graph**, like a
  unit's key, surface and bodies: `Task::Script { unit }`, `Task::Module { unit }`. A unit's key
  task waits for the script runs and modules it depends on. There are no phases and no nested
  requests.
- The certificate is keyed by the graph **as resolved**, before any script runs: packages,
  target kinds, features, the target, the profile. Its shape holds every outside read of the
  request, each script run's declared reads and each expansion's among them. So a no-op is the
  leaf sweep and one certificate lookup, and looks up no script and no module.
- **A crate's identity** (its owner, the first bytes of every id of a fact it owns) is its
  package, target kind, features, target and its dependencies' identities: what the resolved
  graph says, nothing a script prints. A script's output is the **reading unit's own input**:
  - cfg it prints and env it sets are in that unit's `UnitInputs` (cfg atoms from scripts beside
    the late values), so they move the unit's text key and nothing above it;
  - a file it writes to `OUT_DIR` that the unit includes is a file of the unit: imaged, keyed by
    its bytes, its items rows like any other;
  - link arguments are the program's.
- A script's run is keyed by its program's code key and what it is given (`ScriptInputs`: env,
  `DEP_*` from the scripts below it, arguments), all known when its task starts, since it
  waits for what it depends on. Its program is built in the same request (plans, code, link).
- A script that asks `$RUSTC` to compile a probe is answered by the spawn effect's shim, as the
  daemon's sandbox answers it today; the probe's result is in the run's outside shape.

**Floor.** No-op: the sweep and one certificate. An edit that touches no script and no macro
crate: nothing of scripts or modules is looked at below a certificate miss except each unit's
mark. An edit to a macro crate: its module again, then each expansion asked under the new module.

## An expansion is a fact of the macro crate

- A new fact kind, owned by the macro crate's unit. Its id is the call: the export, the input
  tokens with spans made relative, the attribute, the call context. **Not the module.** Its
  digest is the output's. The host answers it under the module this request has for that crate:
  module digest and call id name the `Expansion` record; no record means the expander runs.
  So an edited macro crate gives a new answer under the same id, and every record that asked it
  is found stale, as for any other fact.
- A surface or body task meets the call while it works and needs the tokens to go on, so the
  host gives the task a capability beside its `Universe`: `expand(id, input) -> tokens`. Rules:
  - it never waits (two workers may run the same call; determinism makes their results equal,
    and the driver keeps one);
  - what it finds or makes goes back with the task's result and is filed by the driver's own
    `Put`, so a request is still one commit and the simulation's oracle sees every put;
  - it follows the memo policy: under `Audit` the expander is run again and compared;
  - the ask lands in the task's shape like any ask.
- An expander that keeps state between calls (`blaze-expansion/src/chain.rs`) is refused by
  name until it has a design.
- Declarative macros expand in the task. Their definition is a fact of its owner's surface. No
  record per call: a lookup costs more than the expansion.
- Not chosen: the task returns the expansions it lacks and is run again. That is one more run
  of a crate's name resolution per nesting depth on a cold surface, and the same again for a
  body. The capability costs the seam one sentence ("a task reads no store"), which becomes "a
  task reads the store only through the host's `expand`, whose answers are in its shape".

## Code: the critical path, and the largest piece

- A body record holds generic MIR. Code for an instance also needs what only concrete types
  give: the impl chosen, layouts, vtables, drop glue and shims (which have no body), and the
  code identities of what it calls. So:
  - a **plan** (one instance's description) is a record keyed like a body: by what it is (the
    item, the type arguments, the settings) and filed at the address of what it read (the
    facts at those types, the bodies it took in). `blaze-program`'s `InstancePlan` as a
    key-only record goes;
  - a plan asks for bodies of units whose bodies are not checked (std's), which the driver
    runs on demand as it does a made fact's maker;
  - **code** is one record type, the one blaze-link reads in place. blaze-codegen's
    `CompiledFunction` and blaze-program's `InstanceCode` become that one.
- The bridge is per function, below blaze-codegen's driver: `Plan` is one instance's describe,
  `Code` is prepare and emit, each over a snapshot opened from records, never over a program
  lowered in memory. The dev tier first, with no inlining and no devirtualization.
- Linking is blaze-link: it has TLS, unwind tables, debug info and native objects, and patches
  in place. Its state, in memory only today, becomes the `ProgramManifest`'s payload, so a
  later command patches and does not relink. blaze-program keeps the walk, the keys and the
  deltas; its four-section placement and `Sink` go.
- This piece is larger than the rest of this page together, and scripts and macro modules both
  wait on it (a script is a program; a module is a program for `wasm32-wasip1`, which also
  needs std's records for that target and a wasm link).

## The driver

- `Task` gains `Script`, `Module`, `Plan` and `Code`. An expansion is not a task.
- One new effect: a sandboxed spawn, with the compiler shim for probes.
- A key task can wait on other tasks (its unit's script run and modules).
- Body records asked on demand by a plan, beside made facts asked by a body.
- blaze-expansion's machine stops doing IO: its lookups and puts become the driver's effects.

## Order

1. `check --no-daemon` on hello-world. (Being built.) With it: a no-op gathers nothing (late
   tables from the kept manifests of units whose files did not move); cfg out of the crate key.
2. **The code path**, the critical one: plans keyed by what they read, the per-function bridge,
   one code record, blaze-link from records with its state kept. Proof: `build --no-daemon` of
   hello-world, dev tier.
3. Script nodes: the spawn effect, the declared reads, `OUT_DIR` files as unit files. Proof:
   one std-only build script built and run with no daemon.
4. Module nodes and `expand`: std for wasm, wasm code and link. Proof: `check --no-daemon` of
   ripgrep, and its no-op and body-edit rows in the smoke, before its own code is built.
5. ripgrep, reth, tempo and blaze by the smoke's rows against the daemon's on the same machine.
   The daemon path is deleted where the command's rows are better on all of them.

## Checks

- The simulation holds every new task to a from-scratch build, with a plant per way a record
  could be served stale, each modelled from this page and not from the code: a script's env
  left out of its run's key; a declared read not asked; an expansion answered under another
  module; a plan found after a layout it read moved; code found after a body it took in moved;
  a unit above a script re-keyed when only the script's cfg for a unit below moved.
- ripgrep's rows in the smoke: no-op, body edit, restart, restart then edit, peak footprint,
  each with its floor and ratio.
