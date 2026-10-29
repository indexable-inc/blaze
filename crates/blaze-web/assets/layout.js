// Where each function starts: crates packed as circles, each crate's modules packed inside
// it, each module's functions on a sunflower disk. Area follows the count, so a crate of
// 20k functions is 20k times the space of one function. The force layout refines this; the
// cluster force keeps each module near the centre it gets here.

const GOLDEN = Math.PI * (3 - Math.sqrt(5));

// Circles packed side by side, largest first, by the front-chain algorithm of Wang et al.
// (2006) as d3-hierarchy's packSiblings does it (ISC licence). Returns centres around (0, 0)
// and the radius of the whole.
export function packCircles(radii, pad) {
  const order = radii.map((_, i) => i).sort((a, b) => radii[b] - radii[a]);
  const cs = order.map((i) => ({ i, r: radii[i] + pad / 2, x: 0, y: 0 }));
  frontChain(cs);
  let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
  for (const c of cs) {
    x0 = Math.min(x0, c.x - c.r); x1 = Math.max(x1, c.x + c.r);
    y0 = Math.min(y0, c.y - c.r); y1 = Math.max(y1, c.y + c.r);
  }
  const mx = (x0 + x1) / 2, my = (y0 + y1) / 2;
  const cx = new Float64Array(radii.length);
  const cy = new Float64Array(radii.length);
  let outer = 0;
  for (const c of cs) {
    cx[c.i] = c.x - mx;
    cy[c.i] = c.y - my;
    outer = Math.max(outer, Math.hypot(cx[c.i], cy[c.i]) + radii[c.i]);
  }
  return { cx, cy, outer };
}

function frontChain(circles) {
  const n = circles.length;
  if (!n) return;
  let a = circles[0], b, c;
  a.x = 0; a.y = 0;
  if (n === 1) return;
  b = circles[1];
  a.x = -b.r; b.x = a.r; b.y = 0;
  if (n === 2) return;
  place(b, a, (c = circles[2]));
  let na = { c: a }, nb = { c: b }, nc = { c };
  na.next = nc.prev = nb;
  nb.next = na.prev = nc;
  nc.next = nb.prev = na;
  pack: for (let i = 3; i < n; i++) {
    c = circles[i];
    place(na.c, nb.c, c);
    nc = { c };
    let j = nb.next, k = na.prev, sj = nb.c.r, sk = na.c.r;
    do {
      if (sj <= sk) {
        if (intersects(j.c, c)) {
          nb = j; na.next = nb; nb.prev = na; i--;
          continue pack;
        }
        sj += j.c.r; j = j.next;
      } else {
        if (intersects(k.c, c)) {
          na = k; na.next = nb; nb.prev = na; i--;
          continue pack;
        }
        sk += k.c.r; k = k.prev;
      }
    } while (j !== k.next);
    nc.prev = na; nc.next = nb;
    na.next = nb.prev = nb = nc;
    let best = score(na);
    for (let m = nc.next; m !== nb; m = m.next) {
      const s = score(m);
      if (s < best) { na = m; best = s; }
    }
    nb = na.next;
  }
}

// c tangent to both a and b.
function place(b, a, c) {
  const dx = b.x - a.x, dy = b.y - a.y, d2 = dx * dx + dy * dy;
  if (d2) {
    const a2 = (a.r + c.r) ** 2, b2 = (b.r + c.r) ** 2;
    if (a2 > b2) {
      const x = (d2 + b2 - a2) / (2 * d2), y = Math.sqrt(Math.max(0, b2 / d2 - x * x));
      c.x = b.x - x * dx - y * dy;
      c.y = b.y - x * dy + y * dx;
    } else {
      const x = (d2 + a2 - b2) / (2 * d2), y = Math.sqrt(Math.max(0, a2 / d2 - x * x));
      c.x = a.x + x * dx - y * dy;
      c.y = a.y + x * dy + y * dx;
    }
  } else {
    c.x = a.x + c.r;
    c.y = a.y;
  }
}

function intersects(a, b) {
  const dr = a.r + b.r - 1e-6, dx = b.x - a.x, dy = b.y - a.y;
  return dr > 0 && dr * dr > dx * dx + dy * dy;
}

function score(node) {
  const a = node.c, b = node.next.c, ab = a.r + b.r;
  const dx = (a.x * b.r + b.x * a.r) / ab, dy = (a.y * b.r + b.y * a.r) / ab;
  return dx * dx + dy * dy;
}

// `group` is the module of each node and `groupCrate` the crate of each module. Returns
// node positions scaled to fit [margin, size - margin]^2, and each module's centre on the
// same scale.
export function hierarchicalLayout(n, group, nGroups, groupCrate, nCrates, size, margin = 0.04) {
  const groupCount = new Uint32Array(nGroups);
  for (let i = 0; i < n; i++) groupCount[group[i]]++;
  const spacing = 1;
  const groupRadius = Array.from(groupCount, (c) => spacing * Math.sqrt(Math.max(c, 1)) + spacing);

  const crateGroups = Array.from({ length: nCrates }, () => []);
  for (let g = 0; g < nGroups; g++) crateGroups[groupCrate[g]].push(g);
  const gx = new Float64Array(nGroups);
  const gy = new Float64Array(nGroups);
  const crateRadius = new Array(nCrates).fill(spacing);
  for (let c = 0; c < nCrates; c++) {
    const gs = crateGroups[c];
    if (gs.length === 0) continue;
    const p = packCircles(gs.map((g) => groupRadius[g]), spacing * 3);
    gs.forEach((g, k) => { gx[g] = p.cx[k]; gy[g] = p.cy[k]; });
    crateRadius[c] = p.outer;
  }
  const top = packCircles(crateRadius, spacing * 20);
  for (let g = 0; g < nGroups; g++) {
    gx[g] += top.cx[groupCrate[g]];
    gy[g] += top.cy[groupCrate[g]];
  }

  const scale = ((1 - 2 * margin) * size) / (2 * top.outer);
  const mid = size / 2;
  const pos = new Float32Array(2 * n);
  const seen = new Uint32Array(nGroups);
  for (let i = 0; i < n; i++) {
    const g = group[i];
    const k = seen[g]++;
    const r = spacing * Math.sqrt(k + 0.5);
    const a = k * GOLDEN;
    pos[2 * i] = mid + (gx[g] + r * Math.cos(a)) * scale;
    pos[2 * i + 1] = mid + (gy[g] + r * Math.sin(a)) * scale;
  }
  const centres = new Float32Array(2 * nGroups);
  for (let g = 0; g < nGroups; g++) {
    centres[2 * g] = mid + gx[g] * scale;
    centres[2 * g + 1] = mid + gy[g] * scale;
  }
  const crateCentres = new Float32Array(2 * nCrates);
  for (let c = 0; c < nCrates; c++) {
    crateCentres[2 * c] = mid + top.cx[c] * scale;
    crateCentres[2 * c + 1] = mid + top.cy[c] * scale;
  }
  return { pos, centres, crateCentres, crateRadius: crateRadius.map((r) => r * scale), scale };
}
