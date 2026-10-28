// The module of a function from how a backtrace names it: the path minus its last segment.
//   anim::scene::render                                  anim::scene
//   anim::main::{closure#0}                              anim
//   core::ptr::drop_in_place::<anim::Thing>              core::ptr
//   <alloc::vec::Vec<T> as core::ops::drop::Drop>::drop  alloc::vec::Vec   (the self type's path)
//   <anim::Mesh>::new                                    anim::Mesh
//   <&[u8] as core::fmt::Debug>::fmt                     the crate (a slice has no path)
//   main                                                 the crate

// Splits at the `::` that are outside <>, (), [] and {}.
export function splitPath(s) {
  const out = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === "<" || c === "(" || c === "[" || c === "{") depth++;
    else if ((c === ">" && s[i - 1] !== "-") || c === ")" || c === "]" || c === "}") depth = Math.max(0, depth - 1);
    else if (depth === 0 && c === ":" && s[i + 1] === ":") {
      out.push(s.slice(start, i));
      start = i + 2;
      i++;
    }
  }
  out.push(s.slice(start));
  return out;
}

// The index of the `>` closing the `<` at `open`, or -1.
function closing(s, open) {
  let depth = 0;
  for (let i = open; i < s.length; i++) {
    if (s[i] === "<") depth++;
    else if (s[i] === ">" && s[i - 1] !== "-" && --depth === 0) return i;
  }
  return -1;
}

// A type's path without its generic arguments: `alloc::vec::Vec<T>` is alloc::vec::Vec.
function typePath(t) {
  let s = t.trim();
  for (;;) {
    const before = s;
    s = s.replace(/^&(mut\s+)?/, "").replace(/^\*(const|mut)\s+/, "").replace(/^dyn\s+/, "").replace(/^'\w+\s+/, "").trim();
    if (s === before) break;
  }
  if (s.startsWith("<")) {
    const end = closing(s, 0);
    return end < 0 ? [] : selfPath(s.slice(1, end));
  }
  if (!/^[A-Za-z_]/.test(s) || /^(unsafe\s+)?(extern\s+"[^"]*"\s+)?fn\s*\(/.test(s)) return [];
  return segments(s);
}

// `X as Trait` or `X` inside a qualified path.
function selfPath(inner) {
  let depth = 0;
  for (let i = 0; i < inner.length; i++) {
    const c = inner[i];
    if (c === "<" || c === "(" || c === "[") depth++;
    else if ((c === ">" && inner[i - 1] !== "-") || c === ")" || c === "]") depth--;
    else if (depth === 0 && inner.startsWith(" as ", i)) return typePath(inner.slice(0, i));
  }
  return typePath(inner);
}

// The named segments of a path: no `<...>` turbofish, no generic arguments, no `{closure#0}`.
function segments(s) {
  const out = [];
  for (const seg of splitPath(s)) {
    if (seg === "" || seg.startsWith("<") || seg.startsWith("{")) continue;
    const lt = seg.indexOf("<");
    out.push(lt >= 0 ? seg.slice(0, lt) : seg);
  }
  return out;
}

export function moduleOf(name, krate) {
  if (name.startsWith("<")) {
    const end = closing(name, 0);
    if (end > 0) {
      const p = selfPath(name.slice(1, end));
      return p.length ? p.join("::") : krate;
    }
    return krate;
  }
  const segs = segments(name);
  if (segs.length <= 1) return krate;
  return segs.slice(0, -1).join("::");
}

// A function's own name, as a Rebuilt record says it: `<alloc::vec::Vec<T>>::push` is `push`,
// `core::ptr::drop_in_place::<anim::Thing>` is `drop_in_place`. A closure, which no Rebuilt
// names, is its `{closure#0}`, so that it is not taken for the function it is in.
export function lastName(name) {
  const parts = splitPath(name);
  while (parts.length > 1 && parts[parts.length - 1].startsWith("<")) parts.pop();
  const last = parts[parts.length - 1];
  const lt = last.startsWith("{") ? -1 : last.indexOf("<");
  return lt > 0 ? last.slice(0, lt) : last;
}

// The last named segment, for a short label.
export function shortName(name) {
  const parts = splitPath(name);
  let i = parts.length - 1;
  while (i > 0 && parts[i].startsWith("{")) i--;
  return parts.slice(i).join("::");
}
