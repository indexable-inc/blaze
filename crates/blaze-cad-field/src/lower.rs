//! THE COMPILER: a graph's root to a [`Tape`].
//!
//! One walk over the DAG with an explicit stack (a left-deep union of 65,536 swept segments
//! needs no thread stack), memoized by `(node, the registers of its coordinates)`: a node
//! reached twice in one frame is lowered once, and two parts at one placement share their
//! coordinate rows. Each node emits its formula (the node table in the README) into the
//! canonical emitter (`tape.rs`), which folds constants and hash-conses as it goes; the result
//! is emitted once more from the root (dead-code removal and the canonical order) and given its
//! register program.
//!
//! COST: linear. Each `(node, frame)` pair is lowered once, with a bounded number of
//! instructions per node (at most about 40, for the cone), one hash probe for each; then one
//! pass to re-emit and one to allocate registers. A graph of `n` nodes reached in `f` distinct
//! frames costs `O(n f)` probes; `f` is 1 for every node but those under a repeat (2: the
//! nearest copy and its neighbour), so a model's tape grows linearly in its nodes.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_math::{Aabb3, Affine3, Mat3, Vec3};
use blaze_own::planted::Planted;
use blaze_own::{AVec, Alloc, Vec};
use blaze_units::{Angle, Plain};

use crate::Limits;
use crate::error::CompileError;
use crate::graph::{CanonProfile, CanonSelect, Graph, SplineSpan, m, margin_of};
use crate::node::{Boolean, Detail, Edge, LoftContinuity, Node, NodeId, ProfileId, Shape};
use crate::placement::LINEAR_GRID;
use crate::plant::CadPlant;
use crate::profile::Closed;
use crate::tape::{self, Emit, Instr, Op, Rw, Source, TableKind, Tape};

type R = Result<u32, CompileError>;

/// What [`compile`] takes besides the graph and the root.
#[blaze::args]
#[derive(Clone, Copy)]
pub struct CompileOpts<'a> {
    /// Where the tape lives.
    pub alloc: Alloc<'a>,
    /// Whether detail nodes are included.
    pub detail: Detail = Detail::Full,
    /// The tape's bounds.
    pub limits: Limits = Limits::SHIPPED,
}

/// The tape of `root` (see the module). The tape's level is the root's; its box the root's box.
pub fn compile<'a, P: Planted<CadPlant>>(g: &Graph<'_, P>, root: NodeId, o: CompileOpts<'a>) -> Result<Tape<'a>, CompileError> {
    if !g.has(root) {
        return Err(CompileError::Foreign { id: root.0 });
    }
    let plant = g.plant();
    let mut e = Emit::new(o.alloc, &o.limits);
    let x = e.leaf(Op::X)?;
    let y = e.leaf(Op::Y)?;
    let z = e.leaf(Op::Z)?;
    let mut memo: Map<(NodeId, [u32; 3]), u32, FixedState, Alloc<'a>> = Map::new(MapOpts { hasher: FixedState, alloc: o.alloc, capacity: None });
    let mut stack: AVec<'a, Frame> = Vec::new_in(o.alloc);
    stack.push(Frame::new(root, [x, y, z]));
    while let Some(top) = stack.last().copied() {
        if !top.ready {
            if memo.get(&(top.id, top.at)).is_some() {
                stack.pop();
                continue;
            }
            let mut f = top;
            pre(g, &mut e, &mut f, plant, o.detail)?;
            f.ready = true;
            if let Some(t) = stack.last_mut() {
                *t = f;
            }
            for k in (0..f.n).rev() {
                if memo.get(&f.kids[k]).is_none() {
                    stack.push(Frame::new(f.kids[k].0, f.kids[k].1));
                }
            }
            continue;
        }
        stack.pop();
        let mut vals = [0u32; 2];
        for (v, kid) in vals.iter_mut().zip(top.kids[..top.n].iter()) {
            // Every kid was pushed above this frame and lowered before it is popped again.
            let Some(kv) = memo.get(kid) else { unreachable!("a kid of a ready frame is lowered first") };
            *v = *kv;
        }
        let v = post(g, &mut e, &top, vals, plant, o.detail)?;
        memo.insert((top.id, top.at), v);
    }
    let Some(r) = memo.get(&(root, [x, y, z])).copied() else { unreachable!("the root frame is lowered last") };
    tape::rebuild(Source::of_emit(&e), r, |_, _| Rw::Keep, g.level(root), g.aabb(root), o.limits, o.alloc, plant)
}

/// One node to lower in one frame.
#[derive(Clone, Copy)]
struct Frame {
    id: NodeId,
    at: [u32; 3],
    ready: bool,
    kids: [(NodeId, [u32; 3]); 2],
    n: usize,
    aux: u32,
}

impl Frame {
    fn new(id: NodeId, at: [u32; 3]) -> Frame {
        Frame { id, at, ready: false, kids: [(id, at); 2], n: 0, aux: 0 }
    }
}

/// The instruction helpers the formulas are written with.
impl Emit<'_> {
    fn leaf(&mut self, op: Op) -> R {
        self.push(Instr { op, args: [0; 4], extra: 0 })
    }
    fn k(&mut self, v: f64) -> R {
        self.pooled(Op::Const, [0; 4], &[v])
    }
    fn un(&mut self, op: Op, a: u32) -> R {
        self.push(Instr { op, args: [a, 0, 0, 0], extra: 0 })
    }
    fn bin(&mut self, op: Op, a: u32, b: u32) -> R {
        self.push(Instr { op, args: [a, b, 0, 0], extra: 0 })
    }
    fn sel(&mut self, a: u32, b: u32, c: u32, d: u32) -> R {
        self.push(Instr { op: Op::Select, args: [a, b, c, d], extra: 0 })
    }
    fn row(&mut self, at: [u32; 3], k: [f64; 4]) -> R {
        if k == [1.0, 0.0, 0.0, 0.0] {
            return Ok(at[0]);
        }
        if k == [0.0, 1.0, 0.0, 0.0] {
            return Ok(at[1]);
        }
        if k == [0.0, 0.0, 1.0, 0.0] {
            return Ok(at[2]);
        }
        self.pooled(Op::Row, [at[0], at[1], at[2], 0], &k)
    }
    fn bbox(&mut self, at: [u32; 3], b: &Aabb3) -> R {
        let (c, h) = (b.center(), b.half());
        self.pooled(Op::BoxBound, [at[0], at[1], at[2], 0], &[c.x, c.y, c.z, h.x, h.y, h.z])
    }
    fn addk(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Add, a, k)
    }
    fn subk(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Sub, a, k)
    }
    fn mulk(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Mul, a, k)
    }
    fn divk(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Div, a, k)
    }
    fn maxk(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Max, a, k)
    }
    fn mink(&mut self, a: u32, v: f64) -> R {
        let k = self.k(v)?;
        self.bin(Op::Min, a, k)
    }
    fn clampk(&mut self, a: u32, lo: f64, hi: f64) -> R {
        let t = self.mink(a, hi)?;
        self.maxk(t, lo)
    }
    fn len2(&mut self, a: u32, b: u32) -> R {
        let (aa, bb) = (self.un(Op::Square, a)?, self.un(Op::Square, b)?);
        let s = self.bin(Op::Add, aa, bb)?;
        self.un(Op::Sqrt, s)
    }
    fn len3(&mut self, a: u32, b: u32, c: u32) -> R {
        let (aa, bb, cc) = (self.un(Op::Square, a)?, self.un(Op::Square, b)?, self.un(Op::Square, c)?);
        let s = self.bin(Op::Add, aa, bb)?;
        let s = self.bin(Op::Add, s, cc)?;
        self.un(Op::Sqrt, s)
    }
    /// The exact distance of the 2-D corner region `max(dx, dy) <= 0`:
    /// `min(max(dx, dy), 0) + |max((dx, dy), 0)|`.
    fn corner2(&mut self, dx: u32, dy: u32) -> R {
        let inn = self.bin(Op::Max, dx, dy)?;
        let inn = self.mink(inn, 0.0)?;
        let (ox, oy) = (self.maxk(dx, 0.0)?, self.maxk(dy, 0.0)?);
        let out = self.len2(ox, oy)?;
        self.bin(Op::Add, inn, out)
    }
    /// `(|u|^e + |v|^e (+ |w|^e))^(1/e)` divided into the unit form; `e >= 1`.
    fn e_norm(&mut self, parts: &[u32], e: f64) -> R {
        let mut sum = None;
        for p in parts {
            let a = self.un(Op::Abs, *p)?;
            let t = self.pooled(Op::PowConst, [a, 0, 0, 0], &[e])?;
            sum = Some(match sum {
                None => t,
                Some(s) => self.bin(Op::Add, s, t)?,
            });
        }
        let Some(s) = sum else { unreachable!("e_norm of no parts") };
        self.pooled(Op::PowConst, [s, 0, 0, 0], &[1.0 / e])
    }
}

/// The field of profile `pid` at `(u, v)`.
fn profile2d<P: Planted<CadPlant>>(g: &Graph<'_, P>, e: &mut Emit<'_>, pid: ProfileId, u: u32, v: u32) -> R {
    let rec = *g.profile_rec(pid);
    match rec.closed {
        Some(Closed::Circle { r }) => {
            let l = e.len2(u, v)?;
            e.subk(l, m(r))
        }
        Some(Closed::Rect { hw, hh, round }) => {
            let (au, av) = (e.un(Op::Abs, u)?, e.un(Op::Abs, v)?);
            let dx = e.subk(au, m(hw - round))?;
            let dy = e.subk(av, m(hh - round))?;
            let c = e.corner2(dx, dy)?;
            e.subk(c, m(round))
        }
        Some(Closed::Superellipse { hw, hh, e_ppm }) => {
            let ex = e_ppm as f64 / 1e6;
            let (su, sv) = (e.divk(u, m(hw))?, e.divk(v, m(hh))?);
            let n = e.e_norm(&[su, sv], ex)?;
            let n1 = e.subk(n, 1.0)?;
            let lip = blaze_math::pow(2.0, 1.0 / ex - 0.5).max(1.0);
            e.mulk(n1, m(hw.min(hh)) / lip)
        }
        Some(Closed::Path { .. }) | None => {
            let rows = g.seg_rows(&rec);
            let t = e.table(TableKind::Path2, u64::from(pid.0), [0.0; 8], rows);
            e.push(Instr { op: Op::Path2, args: [u, v, 0, 0], extra: t })
        }
    }
}

/// The local coordinates under the rows `k` (a rigid or affine inverse).
fn rows3(e: &mut Emit<'_>, at: [u32; 3], k: [[f64; 4]; 3]) -> Result<[u32; 3], CompileError> {
    Ok([e.row(at, k[0])?, e.row(at, k[1])?, e.row(at, k[2])?])
}

/// The shape's field at local coordinates `l`.
fn shape(e: &mut Emit<'_>, s: &Shape, l: [u32; 3]) -> R {
    let [x, y, z] = l;
    match *s {
        Shape::HalfSpace => Ok(z),
        Shape::Sphere { r } => {
            let d = e.len3(x, y, z)?;
            e.subk(d, m(r))
        }
        Shape::Box { half, round } => {
            let mut q = [0u32; 3];
            for (qi, (li, hi)) in q.iter_mut().zip(l.iter().zip(half.iter())) {
                let a = e.un(Op::Abs, *li)?;
                *qi = e.subk(a, m(hi - round))?;
            }
            let (ox, oy, oz) = (e.maxk(q[0], 0.0)?, e.maxk(q[1], 0.0)?, e.maxk(q[2], 0.0)?);
            let out = e.len3(ox, oy, oz)?;
            let inn = e.bin(Op::Max, q[1], q[2])?;
            let inn = e.bin(Op::Max, q[0], inn)?;
            let inn = e.mink(inn, 0.0)?;
            let v = e.bin(Op::Add, out, inn)?;
            e.subk(v, m(round))
        }
        Shape::Cylinder { r, half_h } => {
            let q = e.len2(x, y)?;
            let dx = e.subk(q, m(r))?;
            let az = e.un(Op::Abs, z)?;
            let dz = e.subk(az, m(half_h))?;
            e.corner2(dx, dz)
        }
        Shape::Capsule { r, half_h } => {
            let c = e.clampk(z, -m(half_h), m(half_h))?;
            let zz = e.bin(Op::Sub, z, c)?;
            let d = e.len3(x, y, zz)?;
            e.subk(d, m(r))
        }
        Shape::Cone { r0, r1, half_h } => {
            // The exact capped cone: radius `rb` at `z = -h`, `rt` at `z = h`.
            let (rb, rt, h) = (m(r0), m(r1), m(half_h));
            let q = e.len2(x, y)?;
            let zero = e.k(0.0)?;
            let (kb, kt) = (e.k(rb)?, e.k(rt)?);
            let rsel = e.sel(z, zero, kb, kt)?;
            let mq = e.bin(Op::Min, q, rsel)?;
            let cax = e.bin(Op::Sub, q, mq)?;
            let az = e.un(Op::Abs, z)?;
            let cay = e.subk(az, h)?;
            let (k2x, k2y) = (rt - rb, 2.0 * h);
            let kk = k2x * k2x + k2y * k2y;
            let t = e.row([q, z, z], [-k2x / kk, -k2y / kk, 0.0, (rt * k2x + h * k2y) / kk])?;
            let t = e.clampk(t, 0.0, 1.0)?;
            let tx = e.mulk(t, k2x)?;
            let qx = e.subk(q, rt)?;
            let cbx = e.bin(Op::Add, qx, tx)?;
            let ty = e.mulk(t, k2y)?;
            let zy = e.subk(z, h)?;
            let cby = e.bin(Op::Add, zy, ty)?;
            let (a2, b2) = (e.len2(cax, cay)?, e.len2(cbx, cby)?);
            let d = e.bin(Op::Min, a2, b2)?;
            let nd = e.un(Op::Neg, d)?;
            let inner = e.sel(cay, zero, nd, d)?;
            e.sel(cbx, zero, inner, d)
        }
        Shape::Torus { big, small } => {
            let q = e.len2(x, y)?;
            let qx = e.subk(q, m(big))?;
            let d = e.len2(qx, z)?;
            e.subk(d, m(small))
        }
        Shape::Ellipsoid { radii } => {
            let s = [e.divk(x, m(radii[0]))?, e.divk(y, m(radii[1]))?, e.divk(z, m(radii[2]))?];
            let n = e.len3(s[0], s[1], s[2])?;
            let n1 = e.subk(n, 1.0)?;
            e.mulk(n1, m(radii[0].min(radii[1]).min(radii[2])))
        }
        Shape::Superellipsoid { radii, e_ppm } => {
            let ex = e_ppm as f64 / 1e6;
            let s = [e.divk(x, m(radii[0]))?, e.divk(y, m(radii[1]))?, e.divk(z, m(radii[2]))?];
            let n = e.e_norm(&s, ex)?;
            let n1 = e.subk(n, 1.0)?;
            let lip = blaze_math::pow(3.0, 1.0 / ex - 0.5).max(1.0);
            e.mulk(n1, m(radii[0].min(radii[1]).min(radii[2])) / lip)
        }
        Shape::Gyroid { period, thickness } => {
            // g(x,y,z) = sin(kx)cos(ky) + sin(ky)cos(kz) + sin(kz)cos(kx), k = 2*pi / period.
            // ||grad g|| <= k * sqrt(3) everywhere (exact tight bound by Cauchy-Schwarz), so
            // |g| / (k * sqrt(3)) - thickness / 2 is strictly 1-Lipschitz.
            let k = 2.0 * core::f64::consts::PI / m(period);
            let scale = 1.0 / (k * blaze_math::sqrt(3.0));
            let (kx, ky, kz) = (e.mulk(x, k)?, e.mulk(y, k)?, e.mulk(z, k)?);
            let (sx, cx) = (e.un(Op::Sin, kx)?, e.un(Op::Cos, kx)?);
            let (sy, cy) = (e.un(Op::Sin, ky)?, e.un(Op::Cos, ky)?);
            let (sz, cz) = (e.un(Op::Sin, kz)?, e.un(Op::Cos, kz)?);
            let t0 = e.bin(Op::Mul, sx, cy)?;
            let t1 = e.bin(Op::Mul, sy, cz)?;
            let t2 = e.bin(Op::Mul, sz, cx)?;
            let s = e.bin(Op::Add, t0, t1)?;
            let g = e.bin(Op::Add, s, t2)?;
            let ag = e.un(Op::Abs, g)?;
            let d = e.mulk(ag, scale)?;
            e.subk(d, 0.5 * m(thickness))
        }
    }
}

/// The coordinates the operands are lowered at, emitted.
fn pre<P: Planted<CadPlant>>(g: &Graph<'_, P>, e: &mut Emit<'_>, f: &mut Frame, plant: P, detail: Detail) -> Result<(), CompileError> {
    let _ = (plant, detail);
    let at = f.at;
    let [x, y, z] = at;
    let one = |f: &mut Frame, inner: NodeId, c: [u32; 3]| {
        f.kids[0] = (inner, c);
        f.n = 1;
    };
    match *g.node(f.id) {
        Node::Empty
        | Node::Primitive { .. }
        | Node::Extrude { .. }
        | Node::Revolve { .. }
        | Node::Loft { .. }
        | Node::GuidedLoft { .. }
        | Node::Redistance { .. } => {
            f.n = 0;
        }
        Node::Placed { inner, placement } => {
            let c = rows3(e, at, placement.local_rows())?;
            one(f, inner, c);
        }
        Node::Scale { inner, factor_pp12 } => {
            let s = factor_pp12 as f64 / 1e12;
            let c = [e.divk(x, s)?, e.divk(y, s)?, e.divk(z, s)?];
            one(f, inner, c);
        }
        Node::Affine { inner, key, .. } => {
            let a = affine_of(&key);
            let inv = a.inverse_linear();
            let t = (inv.apply(a.shift())).to_array();
            let mut k = [[0.0; 4]; 3];
            for (i, row) in k.iter_mut().enumerate() {
                *row = [inv.rows[i][0], inv.rows[i][1], inv.rows[i][2], -t[i]];
            }
            let c = rows3(e, at, k)?;
            one(f, inner, c);
        }
        Node::MirrorFold { inner, axis } => {
            let mut c = at;
            c[axis.index()] = e.un(Op::Abs, at[axis.index()])?;
            one(f, inner, c);
        }
        Node::Repeat { inner, axis, step, count, extent } => {
            let i = axis.index();
            let (s, last) = (m(step), f64::from(count - 1));
            let u = at[i];
            let q = e.divk(u, s)?;
            let r = e.un(Op::Round, q)?;
            let kc = e.clampk(r, 0.0, last)?;
            let ks = e.mulk(kc, s)?;
            let l = e.bin(Op::Sub, u, ks)?;
            let (zero, mone, pone) = (e.k(0.0)?, e.k(-1.0)?, e.k(1.0)?);
            let sg = e.sel(l, zero, mone, pone)?;
            let nb = e.bin(Op::Add, kc, sg)?;
            let nb = e.clampk(nb, 0.0, last)?;
            let ns = e.mulk(nb, s)?;
            let l2 = e.bin(Op::Sub, u, ns)?;
            // Every copy but these two is at least `s + |l| - w` away along the axis.
            let al = e.un(Op::Abs, l)?;
            let far = e.addk(al, s - m(extent))?;
            let (mut c1, mut c2) = (at, at);
            c1[i] = l;
            c2[i] = l2;
            f.kids = [(inner, c1), (inner, c2)];
            f.n = 2;
            f.aux = far;
        }
        Node::Boolean { a, b, .. } | Node::VariableBoolean { a, b, .. } => {
            f.kids = [(a, at), (b, at)];
            f.n = 2;
        }
        Node::Grow { inner, .. }
        | Node::Erode { inner, .. }
        | Node::VariableOffset { inner, .. }
        | Node::Shell { inner, .. }
        | Node::Detail { inner, .. } => one(f, inner, at),
        Node::Twist { inner, rate, .. } => {
            let w = Angle::<Plain>::microarcsec(rate).as_radians();
            let th = e.mulk(z, w)?;
            let (c, s) = (e.un(Op::Cos, th)?, e.un(Op::Sin, th)?);
            let (xc, ys, yc, xs) = (e.bin(Op::Mul, x, c)?, e.bin(Op::Mul, y, s)?, e.bin(Op::Mul, y, c)?, e.bin(Op::Mul, x, s)?);
            let c = [e.bin(Op::Add, xc, ys)?, e.bin(Op::Sub, yc, xs)?, z];
            one(f, inner, c);
        }
        Node::Bend { inner, radius, .. } => {
            let r = m(radius);
            let rz = e.k(r)?;
            let rz = e.bin(Op::Sub, rz, z)?;
            let rho = e.len2(x, rz)?;
            let th = e.bin(Op::Atan2, x, rz)?;
            let u = e.mulk(th, r)?;
            let rk = e.k(r)?;
            let w = e.bin(Op::Sub, rk, rho)?;
            one(f, inner, [u, y, w]);
        }
        Node::Taper { inner, top_ppm, z0, z1, .. } => {
            let span = m(z1 - z0);
            let k = if span > 0.0 { (top_ppm as f64 / 1e6 - 1.0) / span } else { 0.0 };
            let t = e.subk(z, m(z0))?;
            let t = e.clampk(t, 0.0, span.max(0.0))?;
            let t = e.mulk(t, k)?;
            let s = e.addk(t, 1.0)?;
            let c = [e.bin(Op::Div, x, s)?, e.bin(Op::Div, y, s)?, z];
            one(f, inner, c);
        }
    }
    Ok(())
}

/// The affine map a node's key holds.
pub(crate) fn affine_of(key: &[i64; 12]) -> Affine3 {
    let mut rows = [[0.0; 3]; 3];
    for (i, row) in rows.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = key[i * 3 + j] as f64 * LINEAR_GRID;
        }
    }
    let t = Vec3::new(m(key[9]), m(key[10]), m(key[11]));
    // The graph refused a singular map when it made the node.
    match Affine3::general(Mat3::new(rows), t) {
        Ok(a) => a,
        Err(_) => unreachable!("an affine node's map is invertible (checked by Graph::affine)"),
    }
}

/// The node's value from its operands' values, emitted.
fn post<P: Planted<CadPlant>>(g: &Graph<'_, P>, e: &mut Emit<'_>, f: &Frame, v: [u32; 2], plant: P, detail: Detail) -> R {
    let at = f.at;
    let [x, y, z] = at;
    let node = *g.node(f.id);
    match node {
        Node::Empty => e.k(f64::INFINITY),
        Node::Primitive { shape: s, frame } => {
            let l = if frame.is_identity() {
                at
            } else {
                let mut k = frame.local_rows();
                if plant.has(CadPlant::PlacementShiftDropped) {
                    for r in k.iter_mut() {
                        r[3] = 0.0;
                    }
                }
                rows3(e, at, k)?
            };
            shape(e, &s, l)
        }
        Node::Extrude { profile, half_h } => {
            let d = profile2d(g, e, profile, x, y)?;
            let az = e.un(Op::Abs, z)?;
            let dz = e.subk(az, m(half_h))?;
            e.corner2(d, dz)
        }
        Node::Revolve { profile, offset } => {
            let q = e.len2(x, y)?;
            let u = e.subk(q, m(offset))?;
            profile2d(g, e, profile, u, z)
        }
        Node::Loft { start, len, lipschitz_ppm } => {
            let (z0, _) = g.station(start);
            let (zn, _) = g.station(start + len - 1);
            let zc = e.clampk(z, m(z0), m(zn))?;
            let mut fields: [u32; 2] = [0, 0];
            let mut acc = None;
            for i in 0..len - 1 {
                let (za, pa) = g.station(start + i);
                let (zb, pb) = g.station(start + i + 1);
                fields[0] = if i == 0 { profile2d(g, e, pa, x, y)? } else { fields[1] };
                fields[1] = profile2d(g, e, pb, x, y)?;
                let t = e.subk(zc, m(za))?;
                let t = e.divk(t, m(zb - za))?;
                let dfd = e.bin(Op::Sub, fields[1], fields[0])?;
                let dt = e.bin(Op::Mul, dfd, t)?;
                let b = e.bin(Op::Add, fields[0], dt)?;
                acc = Some(match acc {
                    None => b,
                    Some(prev) => {
                        let ka = e.k(m(za))?;
                        e.sel(zc, ka, prev, b)?
                    }
                });
            }
            let Some(v) = acc else { unreachable!("a loft has at least two stations") };
            let v = e.divk(v, lipschitz_ppm as f64 / 1e6)?;
            let below = e.k(m(z0))?;
            let below = e.bin(Op::Sub, below, z)?;
            let above = e.subk(z, m(zn))?;
            let slab = e.bin(Op::Max, below, above)?;
            e.bin(Op::Max, v, slab)
        }
        Node::GuidedLoft { idx } => lower_guided_loft(g, e, f.id, at, idx),
        Node::Placed { .. } | Node::MirrorFold { .. } => Ok(v[0]),
        Node::Scale { factor_pp12, .. } => e.mulk(v[0], factor_pp12 as f64 / 1e12),
        Node::Affine { lipschitz_ppm, .. } => e.divk(v[0], lipschitz_ppm as f64 / 1e6),
        Node::Repeat { .. } => {
            let t = e.bin(Op::Min, v[0], v[1])?;
            e.bin(Op::Min, t, f.aux)
        }
        Node::Boolean { op, edge, .. } => {
            let (a, b) = match op {
                Boolean::Difference => (v[0], e.un(Op::Neg, v[1])?),
                _ => (v[0], v[1]),
            };
            let union = op == Boolean::Union;
            let sharp = e.bin(if union { Op::Min } else { Op::Max }, a, b)?;
            match edge {
                Edge::Sharp => Ok(sharp),
                Edge::Round { r } => {
                    let r = m(r);
                    // Order `b - a` for `Min` (union) and `a - b` for `Max` (intersect/difference)
                    // so `Abs(0.0)`'s `+1` subgradient matches the tie-break branch of `Min`/`Max`
                    // and yields the exact C1 analytical gradient even on the symmetry surface `a == b`.
                    let d = if union { e.bin(Op::Sub, b, a)? } else { e.bin(Op::Sub, a, b)? };
                    let d = e.un(Op::Abs, d)?;
                    let rk = e.k(r)?;
                    let h = e.bin(Op::Sub, rk, d)?;
                    let h = e.maxk(h, 0.0)?;
                    let h = e.divk(h, r)?;
                    let h2 = e.un(Op::Square, h)?;
                    let bump = e.mulk(h2, r * 0.25)?;
                    let down = union != plant.has(CadPlant::SmoothUnionWrongSign);
                    e.bin(if down { Op::Sub } else { Op::Add }, sharp, bump)
                }
                Edge::Chamfer { r } => {
                    let s = e.bin(Op::Add, a, b)?;
                    let s = e.addk(s, if union { -m(r) } else { m(r) })?;
                    let s = e.mulk(s, 0.5)?;
                    e.bin(if union { Op::Min } else { Op::Max }, sharp, s)
                }
            }
        }
        Node::VariableBoolean { op, chamfer, field, .. } => {
            let (a, b) = match op {
                Boolean::Difference => (v[0], e.un(Op::Neg, v[1])?),
                _ => (v[0], v[1]),
            };
            let union = op == Boolean::Union;
            let sharp = e.bin(if union { Op::Min } else { Op::Max }, a, b)?;
            let (r_eff, l_r) = lower_var_radius(g, e, at, field)?;
            if chamfer {
                let sum = e.bin(Op::Add, a, b)?;
                let shifted = if union { e.bin(Op::Sub, sum, r_eff)? } else { e.bin(Op::Add, sum, r_eff)? };
                let diag = e.mulk(shifted, 0.5)?;
                let raw = e.bin(if union { Op::Min } else { Op::Max }, sharp, diag)?;
                e.divk(raw, 1.0 + 0.5 * l_r)
            } else {
                let d = if union { e.bin(Op::Sub, b, a)? } else { e.bin(Op::Sub, a, b)? };
                let d = e.un(Op::Abs, d)?;
                let r_safe = e.maxk(r_eff, 1e-12)?;
                let num = e.bin(Op::Sub, r_eff, d)?;
                let h = e.bin(Op::Div, num, r_safe)?;
                let h = e.clampk(h, 0.0, 1.0)?;
                let h2 = e.un(Op::Square, h)?;
                let bump = e.bin(Op::Mul, r_eff, h2)?;
                let bump = e.mulk(bump, 0.25)?;
                let down = union != plant.has(CadPlant::SmoothUnionWrongSign);
                let raw = e.bin(if down { Op::Sub } else { Op::Add }, sharp, bump)?;
                e.divk(raw, 1.0 + 0.25 * l_r)
            }
        }
        Node::Grow { r, .. } => e.subk(v[0], m(r)),
        Node::Erode { r, .. } => e.addk(v[0], m(r)),
        Node::VariableOffset { erode, field, .. } => {
            let (r_eff, l_r) = lower_var_radius(g, e, at, field)?;
            let raw = if erode { e.bin(Op::Add, v[0], r_eff)? } else { e.bin(Op::Sub, v[0], r_eff)? };
            e.divk(raw, 1.0 + l_r)
        }
        Node::Shell { t, .. } => {
            let n = e.un(Op::Neg, v[0])?;
            let n = e.subk(n, m(t))?;
            e.bin(Op::Max, v[0], n)
        }
        Node::Twist { lipschitz_ppm, .. } | Node::Bend { lipschitz_ppm, .. } | Node::Taper { lipschitz_ppm, .. } => {
            // Inside the grown container the field divided by the map's Lipschitz factor; outside
            // it, the distance bound of the container itself (see `graph::deform`).
            let c = g.aabb(f.id);
            let cg = c.grow(margin_of(&c));
            let scaled = e.divk(v[0], lipschitz_ppm as f64 / 1e6)?;
            let near = e.bbox(at, &cg)?;
            let far = e.bbox(at, &c)?;
            let zero = e.k(0.0)?;
            e.sel(near, zero, scaled, far)
        }
        Node::Detail { amp, period, .. } => {
            if detail == Detail::Shape {
                return Ok(v[0]);
            }
            let (a, kk) = (m(amp), 2.0 * core::f64::consts::PI / m(period));
            let sx = e.mulk(x, kk)?;
            let sx = e.un(Op::Sin, sx)?;
            let sy = e.mulk(y, kk)?;
            let sy = e.un(Op::Sin, sy)?;
            let sz = e.mulk(z, kk)?;
            let sz = e.un(Op::Sin, sz)?;
            let p = e.bin(Op::Mul, sx, sy)?;
            let p = e.bin(Op::Mul, p, sz)?;
            let p = e.mulk(p, a)?;
            let s = e.bin(Op::Add, v[0], p)?;
            e.divk(s, 1.0 + a * kk * blaze_math::sqrt(3.0))
        }
        Node::Redistance { .. } => {
            let Some((grid, values)) = g.grid(f.id) else {
                return Err(CompileError::Unsolved { name: g.name(f.id).copied() });
            };
            let head = [
                grid.origin[0],
                grid.origin[1],
                grid.origin[2],
                grid.step,
                grid.band,
                f64::from(grid.dims[0]),
                f64::from(grid.dims[1]),
                f64::from(grid.dims[2]),
            ];
            let t = e.table(TableKind::Grid3, u64::from(f.id.0), head, values);
            e.push(Instr { op: Op::Grid3, args: [x, y, z, 0], extra: t })
        }
    }
}

/// Lowers a quantized piecewise-cubic spline at `z_reg` (already clamped to `[z_lo, z_hi]`)
/// using only existing tape ops (`Const`, `Add`, `Sub`, `Mul`, `Div`, `Min`, `Max`, `Select`).
fn lower_spline(e: &mut Emit<'_>, spans: &[SplineSpan], z_reg: u32, z_lo: f64, z_hi: f64, default_val: f64) -> R {
    if spans.is_empty() {
        return e.k(default_val);
    }
    if spans.len() == 1 && spans[0].c_pp12[1] == 0 && spans[0].c_pp12[2] == 0 && spans[0].c_pp12[3] == 0 {
        return e.k(spans[0].c_pp12[0] as f64 / 1e12);
    }
    let rz0 = m(spans[0].z0_nm);
    let rzn = m(spans[spans.len() - 1].z1_nm);
    let zs = if rz0 <= z_lo && rzn >= z_hi {
        z_reg
    } else {
        e.clampk(z_reg, rz0, rzn)?
    };
    let mut acc = None;
    for sp in spans {
        let za = m(sp.z0_nm);
        let zb = m(sp.z1_nm);
        let h = zb - za;
        let u = e.subk(zs, za)?;
        let u = e.divk(u, h)?;
        let u = e.clampk(u, 0.0, 1.0)?;
        let c0 = sp.c_pp12[0] as f64 / 1e12;
        let c1 = sp.c_pp12[1] as f64 / 1e12;
        let c2 = sp.c_pp12[2] as f64 / 1e12;
        let c3 = sp.c_pp12[3] as f64 / 1e12;
        let v = if sp.c_pp12[3] == 0 && sp.c_pp12[2] == 0 {
            let t1 = e.mulk(u, c1)?;
            e.addk(t1, c0)?
        } else if sp.c_pp12[3] == 0 {
            let t2 = e.mulk(u, c2)?;
            let t2 = e.addk(t2, c1)?;
            let t2 = e.bin(Op::Mul, t2, u)?;
            e.addk(t2, c0)?
        } else {
            let v = e.mulk(u, c3)?;
            let v = e.addk(v, c2)?;
            let v = e.bin(Op::Mul, v, u)?;
            let v = e.addk(v, c1)?;
            let v = e.bin(Op::Mul, v, u)?;
            e.addk(v, c0)?
        };
        acc = Some(match acc {
            None => v,
            Some(prev) => {
                let ka = e.k(za)?;
                e.sel(zs, ka, prev, v)?
            }
        });
    }
    let Some(res) = acc else { unreachable!("non-empty spline spans") };
    Ok(res)
}

/// Lowers [`Node::GuidedLoft`] to existing tape ops.
fn lower_guided_loft<P: Planted<CadPlant>>(g: &Graph<'_, P>, e: &mut Emit<'_>, id: NodeId, at: [u32; 3], idx: u32) -> R {
    let gl = *g.guided_loft_rec(idx);
    let [x, y, z] = at;
    let (z0_nm, _) = g.station(gl.stations_start);
    let (zn_nm, _) = g.station(gl.stations_start + gl.stations_len - 1);
    let (z0, zn) = (m(z0_nm), m(zn_nm));
    let zc = e.clampk(z, z0, zn)?;

    // Center offset `(cx(zc), cy(zc))`.
    let dx = if gl.splines[2].1 == 0 {
        x
    } else {
        let cx = lower_spline(e, g.spline_slice(gl.splines[2]), zc, z0, zn, 0.0)?;
        e.bin(Op::Sub, x, cx)?
    };
    let dy = if gl.splines[3].1 == 0 {
        y
    } else {
        let cy = lower_spline(e, g.spline_slice(gl.splines[3]), zc, z0, zn, 0.0)?;
        e.bin(Op::Sub, y, cy)?
    };

    // Un-twist by `-theta(zc)`.
    let (rx, ry) = if gl.splines[4].1 == 0 {
        (dx, dy)
    } else {
        let th = lower_spline(e, g.spline_slice(gl.splines[4]), zc, z0, zn, 0.0)?;
        let (cs, sn) = (e.un(Op::Cos, th)?, e.un(Op::Sin, th)?);
        let (xc, ys, yc, xs) = (
            e.bin(Op::Mul, dx, cs)?,
            e.bin(Op::Mul, dy, sn)?,
            e.bin(Op::Mul, dy, cs)?,
            e.bin(Op::Mul, dx, sn)?,
        );
        (e.bin(Op::Add, xc, ys)?, e.bin(Op::Sub, yc, xs)?)
    };

    // Scale `(sx(zc), sy(zc))` with safe lower clamp `0.5 * s_min` (no-op for all points,
    // keeps denominator intervals strictly positive).
    let sx = if gl.splines[0].1 == 0 {
        e.k(1.0)?
    } else {
        let raw = lower_spline(e, g.spline_slice(gl.splines[0]), zc, z0, zn, 1.0)?;
        e.maxk(raw, 0.5 * (gl.s_min_pp12[0] as f64 / 1e12))?
    };
    let sy = if gl.splines[1].1 == 0 {
        e.k(1.0)?
    } else {
        let raw = lower_spline(e, g.spline_slice(gl.splines[1]), zc, z0, zn, 1.0)?;
        e.maxk(raw, 0.5 * (gl.s_min_pp12[1] as f64 / 1e12))?
    };
    let u = if gl.splines[0].1 == 0 { rx } else { e.bin(Op::Div, rx, sx)? };
    let v = if gl.splines[1].1 == 0 { ry } else { e.bin(Op::Div, ry, sy)? };

    // Blend station profile fields across each span with requested continuity.
    let mut fields: [u32; 2] = [0, 0];
    let mut acc = None;
    for i in 0..gl.stations_len - 1 {
        let (za, pa) = g.station(gl.stations_start + i);
        let (zb, pb) = g.station(gl.stations_start + i + 1);
        fields[0] = if i == 0 { profile2d(g, e, pa, u, v)? } else { fields[1] };
        fields[1] = profile2d(g, e, pb, u, v)?;
        let t = e.subk(zc, m(za))?;
        let t = e.divk(t, m(zb - za))?;
        let t = e.clampk(t, 0.0, 1.0)?;
        let w = match gl.continuity {
            LoftContinuity::Linear => t,
            LoftContinuity::SmoothC1 => {
                let t2 = e.un(Op::Square, t)?;
                let c = e.mulk(t, -2.0)?;
                let c = e.addk(c, 3.0)?;
                e.bin(Op::Mul, t2, c)?
            }
            LoftContinuity::CurvatureC2 => {
                let t2 = e.un(Op::Square, t)?;
                let t3 = e.bin(Op::Mul, t2, t)?;
                let p = e.mulk(t, 6.0)?;
                let p = e.addk(p, -15.0)?;
                let p = e.bin(Op::Mul, p, t)?;
                let p = e.addk(p, 10.0)?;
                e.bin(Op::Mul, t3, p)?
            }
        };
        let dfd = e.bin(Op::Sub, fields[1], fields[0])?;
        let dt = e.bin(Op::Mul, dfd, w)?;
        let b = e.bin(Op::Add, fields[0], dt)?;
        acc = Some(match acc {
            None => b,
            Some(prev) => {
                let ka = e.k(m(za))?;
                e.sel(zc, ka, prev, b)?
            }
        });
    }
    let Some(d) = acc else { unreachable!("a guided loft has at least two stations") };

    // Geometric distance preservation by `min(sx, sy)` and division by sound Lipschitz bound.
    let ms = e.bin(Op::Min, sx, sy)?;
    let scaled = e.bin(Op::Mul, d, ms)?;
    let body = e.divk(scaled, gl.lipschitz_ppm as f64 / 1e6)?;

    // Cap at `[z0, zn]`, optionally smooth round-intersected with `cap_round`.
    let below = e.k(z0)?;
    let below = e.bin(Op::Sub, below, z)?;
    let above = e.subk(z, zn)?;
    let slab = e.bin(Op::Max, below, above)?;
    let capped = if gl.cap_round == 0 {
        e.bin(Op::Max, body, slab)?
    } else {
        let r = m(gl.cap_round);
        let sharp = e.bin(Op::Max, body, slab)?;
        let diff = e.bin(Op::Sub, body, slab)?;
        let diff = e.un(Op::Abs, diff)?;
        let rk = e.k(r)?;
        let h = e.bin(Op::Sub, rk, diff)?;
        let h = e.maxk(h, 0.0)?;
        let h = e.divk(h, r)?;
        let h2 = e.un(Op::Square, h)?;
        let bump = e.mulk(h2, r * 0.25)?;
        e.bin(Op::Add, sharp, bump)?
    };

    if gl.guarded {
        let c = g.aabb(id);
        let cg = c.grow(margin_of(&c));
        let near = e.bbox(at, &cg)?;
        let far = e.bbox(at, &c)?;
        let zero = e.k(0.0)?;
        e.sel(near, zero, capped, far)
    } else {
        Ok(capped)
    }
}

/// Lowers `r_eff(p) = profile(p) * mask(p)` and returns `(r_eff_reg, L_r)`.
fn lower_var_radius<P: Planted<CadPlant>>(g: &Graph<'_, P>, e: &mut Emit<'_>, at: [u32; 3], field: u32) -> Result<(u32, f64), CompileError> {
    let vf = *g.var_field_rec(field);
    let prof_r = match vf.profile {
        CanonProfile::Constant { r_nm } => e.k(m(r_nm))?,
        CanonProfile::Linear { from_nm, to_nm, r0_nm, r1_nm } => {
            let (fx, fy, fz) = (m(from_nm[0]), m(from_nm[1]), m(from_nm[2]));
            let (dx, dy, dz) = (m(to_nm[0] - from_nm[0]), m(to_nm[1] - from_nm[1]), m(to_nm[2] - from_nm[2]));
            let len2 = dx * dx + dy * dy + dz * dz;
            let inv = 1.0 / len2;
            let t = e.row(at, [dx * inv, dy * inv, dz * inv, -(dx * fx + dy * fy + dz * fz) * inv])?;
            let t = e.clampk(t, 0.0, 1.0)?;
            let dr = e.mulk(t, m(r1_nm - r0_nm))?;
            e.addk(dr, m(r0_nm))?
        }
        CanonProfile::Curve { origin_nm, axis_pp12, start, len } => {
            let (ox, oy, oz) = (m(origin_nm[0]), m(origin_nm[1]), m(origin_nm[2]));
            let (ax, ay, az) = (
                axis_pp12[0] as f64 / 1e12,
                axis_pp12[1] as f64 / 1e12,
                axis_pp12[2] as f64 / 1e12,
            );
            let alen = blaze_math::sqrt(ax * ax + ay * ay + az * az);
            let (ux, uy, uz) = (ax / alen, ay / alen, az / alen);
            let s = e.row(at, [ux, uy, uz, -(ux * ox + uy * oy + uz * oz)])?;
            let stops = g.radius_stops_slice(start, len);
            let sc = e.clampk(s, m(stops[0].0), m(stops[stops.len() - 1].0))?;
            let mut acc = None;
            for i in 0..stops.len() - 1 {
                let (at0, r0) = stops[i];
                let (at1, r1) = stops[i + 1];
                let t = e.subk(sc, m(at0))?;
                let t = e.divk(t, m(at1 - at0))?;
                let t = e.clampk(t, 0.0, 1.0)?;
                let t2 = e.un(Op::Square, t)?;
                let tw = e.mulk(t, -2.0)?;
                let tw = e.addk(tw, 3.0)?;
                let w = e.bin(Op::Mul, t2, tw)?;
                let dr = e.mulk(w, m(r1 - r0))?;
                let val = e.addk(dr, m(r0))?;
                acc = Some(match acc {
                    None => val,
                    Some(prev) => {
                        let ka = e.k(m(at0))?;
                        e.sel(sc, ka, prev, val)?
                    }
                });
            }
            let Some(res) = acc else { unreachable!("curve profile has at least two stops") };
            res
        }
    };

    let r_eff = match vf.select {
        CanonSelect::All => prof_r,
        CanonSelect::Box { lo_nm, hi_nm, fade_nm } => {
            let mut q = [0u32; 3];
            for i in 0..3 {
                let c = 0.5 * (m(lo_nm[i]) + m(hi_nm[i]));
                let h = 0.5 * (m(hi_nm[i]) - m(lo_nm[i]));
                let d = e.subk(at[i], c)?;
                let a = e.un(Op::Abs, d)?;
                q[i] = e.subk(a, h)?;
            }
            let (ox, oy, oz) = (e.maxk(q[0], 0.0)?, e.maxk(q[1], 0.0)?, e.maxk(q[2], 0.0)?);
            let out = e.len3(ox, oy, oz)?;
            let inn = e.bin(Op::Max, q[1], q[2])?;
            let inn = e.bin(Op::Max, q[0], inn)?;
            let inn = e.mink(inn, 0.0)?;
            let sd = e.bin(Op::Add, out, inn)?;
            apply_fade_mask(e, prof_r, sd, m(fade_nm))?
        }
        CanonSelect::Ball { center_nm, radius_nm, fade_nm } => {
            let dx = e.subk(at[0], m(center_nm[0]))?;
            let dy = e.subk(at[1], m(center_nm[1]))?;
            let dz = e.subk(at[2], m(center_nm[2]))?;
            let dist = e.len3(dx, dy, dz)?;
            let sd = e.subk(dist, m(radius_nm))?;
            apply_fade_mask(e, prof_r, sd, m(fade_nm))?
        }
        CanonSelect::Capsule { a_nm, b_nm, radius_nm, fade_nm } => {
            let (ax, ay, az) = (m(a_nm[0]), m(a_nm[1]), m(a_nm[2]));
            let (abx, aby, abz) = (m(b_nm[0] - a_nm[0]), m(b_nm[1] - a_nm[1]), m(b_nm[2] - a_nm[2]));
            let ab2 = abx * abx + aby * aby + abz * abz;
            let dist = if ab2 > 0.0 {
                let inv = 1.0 / ab2;
                let t = e.row(at, [abx * inv, aby * inv, abz * inv, -(abx * ax + aby * ay + abz * az) * inv])?;
                let t = e.clampk(t, 0.0, 1.0)?;
                let pax = e.subk(at[0], ax)?;
                let pay = e.subk(at[1], ay)?;
                let paz = e.subk(at[2], az)?;
                let tx = e.mulk(t, abx)?;
                let ty = e.mulk(t, aby)?;
                let tz = e.mulk(t, abz)?;
                let px = e.bin(Op::Sub, pax, tx)?;
                let py = e.bin(Op::Sub, pay, ty)?;
                let pz = e.bin(Op::Sub, paz, tz)?;
                e.len3(px, py, pz)?
            } else {
                let dx = e.subk(at[0], ax)?;
                let dy = e.subk(at[1], ay)?;
                let dz = e.subk(at[2], az)?;
                e.len3(dx, dy, dz)?
            };
            let sd = e.subk(dist, m(radius_nm))?;
            apply_fade_mask(e, prof_r, sd, m(fade_nm))?
        }
    };
    Ok((r_eff, vf.l_r_ppm as f64 / 1e6))
}

fn apply_fade_mask(e: &mut Emit<'_>, prof_r: u32, sd: u32, fade: f64) -> R {
    let u = e.mulk(sd, -1.0 / fade)?;
    let u = e.addk(u, 1.0)?;
    let u = e.clampk(u, 0.0, 1.0)?;
    let u2 = e.un(Op::Square, u)?;
    let w = e.mulk(u, -2.0)?;
    let w = e.addk(w, 3.0)?;
    let mask = e.bin(Op::Mul, u2, w)?;
    e.bin(Op::Mul, prof_r, mask)
}
