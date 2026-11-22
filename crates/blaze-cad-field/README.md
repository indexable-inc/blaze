---
status: current
summary: The implicit CAD kernel's bottom (D183). A solid is a field, negative inside. It is built as a hash-consed graph of canonical nodes, compiled to a flat tape, and evaluated by three interpreters (points in SIMD, intervals with simplification, gradients). This file is the contract the mesher, the record keys, the picture and the checks are written against.
date: 2026-11-20
---
# blaze-cad-field

`#![no_std]`, no `alloc`, no registry package. Every table lives in a handed `Alloc`, and every
bound is a `Limits` field (`Limits::SHIPPED`). Errors are typed and name the node by its kind
and the author's name.

## The pipeline

```text
Graph (named options, Solid<Level>) --compile--> Tape --eval_point / eval_points--> f64
                                                     --eval_box--> Interval + Choices --simplify--> Tape (a region's)
                                                     --eval_grad--> (f64, Vec3)
                                                     --inspect--> bounds, classify, distance_between, section, ray
```

## Nodes

| node | made by | level | field |
|---|---|---|---|
| `Empty` | `empty` | exact | `+inf` |
| `Primitive { shape, frame }` | `half_space`, `sphere`, `cuboid`, `cylinder`, `capsule`, `cone`, `torus` | exact | the closed-form distance in the frame |
| | `ellipsoid`, `superellipsoid` | bound | `(|p/r| - 1) min r` (divided by `L_e` for a superellipsoid) |
| `Extrude { profile, half_h }` | `extrude` | the profile's | 2D field and slab, exact corner |
| `Revolve { profile, offset }` | `revolve`, and with a wedge `revolve_part`, `sweep_arc` | the profile's (a wedge cut: bound) | the profile at `(|xy| - offset, z)` |
| `Loft { stations, L }` | `loft` | bound | piecewise-linear blend of station fields, `/L`, capped by the end slab |
| `Placed { inner, placement }` | `place` (folded into a primitive's frame when it can be) | kept | `inner(placement^-1 p)` |
| `Scale { inner, factor }` | `scale` | kept | `s inner(p / s)` |
| `Affine { inner, key, L }` | `affine` | bound | `inner(A^-1 (p - t)) / L` |
| `MirrorFold { inner, axis }` | `mirror` | kept | `inner` with `p[axis]` folded to `|p[axis]|` |
| `Repeat { inner, axis, step, count, extent }` | `linear_repeat` | inside from exact or inside, else bound | the nearest copy, its neighbour, and a stated bound for the rest |
| union of placed copies | `circular_repeat` | as a union | a balanced tree of unions |
| `Boolean { op, edge, a, b }` | `union`, `intersect`, `difference`; `round_*`, `chamfer_*` | sharp: by the lattice; blended: bound | min / max; `sharp -+ r h^2 / 4`; `min/max(sharp, (a + b -+ r) / 2)` |
| `Grow`, `Erode` | `grow` (needs at least outside), `erode` (needs at least inside) | outside, inside | `f - r`, `f + r` |
| `Shell { inner, t }` | `shell` (needs exact) | outside | `max(f, -f - t)` |
| `Twist`, `Bend`, `Taper` | `twist`, `bend`, `taper` | bound | `inner(map^-1 p) / L` inside the grown container, the container's bound outside |
| `Detail { inner, amp, period }` | `detail` (needs Lipschitz) | bound | `(f + A sin kx sin ky sin kz) / (1 + A k sqrt 3)`, only at `Detail::Full` |
| `Redistance { inner }` | `redistance`, `round_edges` | exact | the grid the mesher attached (`attach_grid`), trilinear; `max(v - e, e)` outside |

Levels are types: `Solid<Exact>`, `Solid<Outside>`, `Solid<Inside>` and `Solid<Bound>`. The
lattice's associated types make the levels type-check (`union` of two exact solids is
`Solid<Outside>`, and `grow` needs `AtLeastOutside`). Each node also records its `Level`, and the
tape carries it.

**Canonical form before hashing.**
- A node's digest is BLAKE3 (derived key) of its kind, its integer parameters and its operands'
  digests.
- Lengths are whole nanometres and angles whole micro-arcseconds (blaze-units). A ratio is in
  millionths (10^-12 for a scale), and a placement's linear part is on the 2^-40 grid.
- Placement is not geometry. A shared placement is hoisted above a boolean, offset, shell,
  detail or scale, and is never pushed into the leaves. `split_placement` reads it back.
- Commutative operands are ordered by digest, and identities are dropped. Offsets compose,
  scales multiply and placements compose.
- Names are kept beside the graph and are never hashed.

## The tape

Each instruction is 24 bytes: an op byte, four `u32` operand indices (each an earlier
instruction) and one `extra` word. The ops are `X Y Z Const Neg Abs Sqrt Square Sin Cos Round
Add Sub Mul Div Min Max Atan2 PowConst Select Row BoxBound Path2 Grid3`.
- `Row` is `k0 x + k1 y + k2 z + k3`, from the pool.
- `BoxBound` is the distance bound of a box.
- `Path2` is a 2D path's exact distance, from a table.
- `Grid3` is a trilinear grid, from a table.

A register program beside the tape (16 bytes an instruction, `u16` registers, last-use
allocation) is what the interpreters run.

**One emitter** makes every tape: a depth-first post-order walk from the root, hash-consed. It
folds constants with the point interpreter's own functions and applies only the bit-exact
identities. So a tape is a canonical function of its DAG, and `Tape::digest` is a key.

**Encoding** is little-endian, in this order:
1. `BCFT` and the schema digest;
2. the level, the root, and four counts;
3. the box (six f64);
4. the instructions, the pool, the tables and the data.

The digest covers everything except the box, so a region key does not move when a far part
grows. `Tape::decode` checks every byte against `Limits`.

## The interpreters

| call | promise |
|---|---|
| `eval_point(t, p, s)` | the field at `p`; deterministic (blaze-math's functions, no FMA contraction) |
| `eval_points(t, pts, out, lanes, s)` | the same bits as `eval_point` at every point, for `Lanes::One`, `Four`, `Eight` (`core::simd`, scalar remainder) |
| `eval_box(t, box, s)` | an interval holding every point value over the box (outward rounded, every instruction), and one `Choice` for each `Min`, `Max` and `Select` that the box decides |
| `simplify(t, choices, alloc)` | a tape equal BIT FOR BIT to `t` at every point of the box the choices came from; its digest is the region key |
| `eval_grad(t, p, s)` | `eval_point`'s value and the forward-mode gradient (a branch's one-sided gradient at a kink) |
| `bounds`, `classify`, `distance_between`, `section`, `ray` | the box of the solid; inside, outside or on the surface within a tolerance; the gap between two solids settled within a tolerance (or the budget spent, said so); a plane section as a tape; the first hit and normal on a ray |

`Scratch` holds the registers between calls, so a mesher's millions of calls allocate nothing.
The CPU's lane width is the caller's choice; nothing here reads CPU features.

## Cost

Measure with `cargo run --release -p blaze-cad-field --example cost`. It prints points per
second, nanoseconds per point and nanoseconds per point per instruction, for models of 1 to 256
parts and 1, 4 and 8 lanes. It also prints nanoseconds per box, the cost of a simplification and
the share of instructions a surface cell keeps. **No numbers have been measured yet.** The floor
the design aims at (rule 67) is about one fused operation per lane per cycle: roughly 0.1 to
0.3 ns per point per instruction at 8 lanes on a current core. A surface cell of a model with
many parts should keep a constant number of instructions, not a fraction of the model.

## The contract for the next crates

These signatures are what blaze-cad-mesh, the record keys, blaze-cad, the picture and the
checks are written against. The names are proposals and the types are this crate's.

```rust
// blaze-cad-mesh: dual contouring over an octree of cells. Each cell is decided by eval_box,
// a surface cell's tape is simplified, vertices are placed with eval_grad (a QEF solve), and
// Redistance grids are attached back to the graph.
pub struct MeshOpts<'a> { pub alloc: Alloc<'a>, pub tolerance: Length, pub detail: Detail = Detail::Full, pub limits: Limits = Limits::SHIPPED }
pub fn mesh<'a>(t: &Tape<'_>, o: MeshOpts<'a>) -> Result<Mesh<'a>, MeshError>;
pub fn solve_redistance<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, s: Solid<Bound>, o: MeshOpts<'_>) -> Result<(), MeshError>;
```
A mesh is a record. A cell's part of it is keyed by `(simplified tape digest, cell box on the
tolerance grid, tolerance, detail)`. An edit re-meshes only the cells whose simplified tape
changed, and a far edit changes no cell's key (`region_keys_follow_the_change`). The mesher
attaches every `Redistance` grid before compiling the node that needs it (the compiler says
`CompileError::Unsolved` otherwise).

```rust
// Record keys: what a model's caches are keyed by.
pub struct ModelKey { pub root: Digest /* Graph::digest_at(root, detail) */, pub detail: Detail, pub tolerance_nm: i64 }
pub struct RegionKey { pub tape: Digest /* simplify(..).digest() */, pub cell: [i64; 6] /* nm */, pub tolerance_nm: i64 }
```
`Graph::digest_at(id, Detail::Shape)` is the coarse digest. It ignores detail reliefs, so a check
or a coarse mesh is not rebuilt when only a relief changes.

```rust
// blaze-cad: the user-facing model. Named parts, assemblies, materials.
pub struct Part { pub solid: Solid<Bound>, pub placement: Placement, pub name: Name }
pub fn assemble<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, parts: &[Part]) -> Result<Solid<Bound>, CadError>;
```
blaze-cad keeps each part's geometry and its placement apart. `split_placement` gives both, and
moving a part re-keys only its placement node and its ancestors. Mass properties use
`Density`, `Mass` and `Volume` from blaze-units over the mesh or a sampled grid.

```rust
// The picture: sphere tracing on the tape, no mesh needed.
pub fn render(t: &Tape<'_>, camera: &Camera, out: &mut [u32], s: &mut Scratch<'_>) -> Result<(), InspectError>;
```
Each pixel is one `ray` call (sphere tracing needs only a bound: any `Level`), shaded with the
hit's `normal` from `eval_grad`. For speed, a tile can be decided by `eval_box` over the tile's
frustum box and drawn from the tile's simplified tape.

```rust
// The checks: each one a record keyed like its inputs, answering with a typed finding.
pub fn min_wall(t: &Tape<'_>, wall: Length, o: CheckOpts) -> Result<Option<WallFinding>, InspectError>;
pub fn no_interference(a: &Tape<'_>, b: &Tape<'_>, clearance: Length, o: CheckOpts) -> Result<Option<Interference>, InspectError>;
pub fn fits_within(part: &Tape<'_>, envelope: &Tape<'_>, o: CheckOpts) -> Result<Option<Overhang>, InspectError>;
```
- `min_wall`: erode by half the wall and look for points that change sign; equivalently,
  `classify` the eroded field over the solid's cells. It needs an at-least-inside field.
- `no_interference`: `distance_between(a, b)` settled below `clearance` is a finding.
  `Gap::BudgetSpent` is reported as unknown, never as a pass.
- `fits_within`: `bounds` of `part` minus `envelope`; an empty result is a pass.

Each check takes the model's `Detail::Shape` tapes, so reliefs never rerun a check.

## Tests

`src/tests.rs` holds thirteen properties. Each has an oracle and a planted bug its control must
catch (`CadPlant`):
- intervals contain points;
- a simplified tape equals the full tape bit for bit;
- levels hold (stepping onto the surface, blends against sharp booleans, 1-Lipschitz);
- gradients match differences;
- the tape matches a reference evaluator;
- lanes match scalar;
- digests cover every parameter;
- spellings share a digest;
- rotation keeps the geometry digest;
- one change re-keys exactly `k + 2` nodes;
- region keys follow the change;
- far parts drop out of cells (at most 2%);
- tapes round-trip.
