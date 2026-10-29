// A crate's colour is a function of its name alone, so it is the same in every build and on
// every page. The standard library's crates get fixed, quieter colours.

const FIXED = {
  core: [0.42, 0.47, 0.56],
  alloc: [0.50, 0.44, 0.58],
  std: [0.38, 0.53, 0.60],
};

function fnv1a(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

function hsl(h, s, l) {
  const k = (n) => (n + h * 12) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n) => l - a * Math.max(-1, Math.min(k(n) - 3, 9 - k(n), 1));
  return [f(0), f(8), f(4)];
}

export function crateColor(name) {
  if (FIXED[name]) return FIXED[name];
  const h = fnv1a(name);
  const hue = (h % 360) / 360;
  const light = 0.58 + ((h >>> 9) % 10) / 100;
  const sat = 0.62 + ((h >>> 17) % 20) / 100;
  return hsl(hue, sat, light);
}

export function css([r, g, b], a = 1) {
  return `rgba(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)}, ${a})`;
}
