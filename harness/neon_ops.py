#!/usr/bin/env python3
"""neon_ops.py: writes the corpus module `neon_ops.rs`, which calls every function of
`core::arch::aarch64`'s NEON with a const parameter (shifts by an immediate, lanes, `vext`,
fixed-point conversions, ...) at each end of the immediate's range and at its middle.

Those functions are generic, so no build of the standard library compiles them: each is
compiled by whichever crate names it, with its immediate, and a refusal or a miscompile in one
only shows in a program that calls it. The corpus runs them all against rustc's build
(`harness/difftest.py`), on the extensions the machine has.

    python3 harness/neon_ops.py            # from blaze's library/
    python3 harness/neon_ops.py --check    # fails when neon_ops.rs is not what it writes
"""
import argparse, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "corpus", "cases", "src", "neon_ops.rs")
SOURCES = ["aarch64/neon/generated.rs", "aarch64/neon/mod.rs",
           "arm_shared/neon/generated.rs", "arm_shared/neon/mod.rs"]

# The target features `is_aarch64_feature_detected!` takes on the pinned nightly without a
# feature gate of its own. A function that needs another one is left out and counted.
DETECTED = {"neon", "aes", "sha2", "sha3", "sm4", "rdm", "dotprod", "i8mm", "fp16", "fhm",
            "fcma", "frintts", "bf16", "crc", "jsconv", "rcpc3", "faminmax", "lut", "fp8"}
# Gates of the detection of the features not stable on the pinned nightly.
DETECTION_GATES = {"rcpc3", "faminmax", "lut", "fp8"}

# The calls in one function of the module. A refusal skips the function, so
# `NEON_OPS_PER_GROUP=1` gives each call its own, to count what is refused.
CALLS_PER_GROUP = int(os.environ.get("NEON_OPS_PER_GROUP", "120"))


def sysroot():
    """core::arch's source in blaze's library/ (no toolchain's `rust-src` is read)."""
    return os.path.join(os.path.dirname(HERE), "library/stdarch/crates/core_arch/src")


FN = re.compile(r"^pub (unsafe )?fn (\w+)<([^>]*)>\((.*?)\)(?: -> ([^{]+?))?\s*\{", re.S | re.M)


def split_top(text):
    """`text` split at its commas outside brackets."""
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in "([<":
            depth += 1
        elif ch in ")]>":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur.strip())
    return parts


def attributes(text, start):
    """The attributes before the item at `start`, each joined onto one line."""
    lines = text[:start].split("\n")
    out, cur = [], []
    for line in reversed(lines[:-1] if lines[-1] == "" else lines):
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("#[") or stripped.startswith("#![") or cur:
            cur.insert(0, stripped)
            if stripped.startswith("#["):
                out.insert(0, " ".join(cur))
                cur = []
            continue
        if stripped in (")]",) or stripped.endswith(",") or stripped.startswith(("all(", "any(", "not(", "target_arch", "stable(", "unstable(", "assert_instr")):
            cur.insert(0, stripped)
            continue
        break
    return out


def body_of(text, start):
    end = text.find("\n}\n", start)
    return text[start:end]


def ranges(consts, body):
    """Each const's inclusive range, from the `static_assert`s of its body; None when one has
    none."""
    found = {}
    for name in consts:
        n = re.escape(name)
        if m := re.search(rf"static_assert_uimm_bits!\({n}, (\d+)\)", body):
            found[name] = (0, (1 << int(m[1])) - 1)
        elif m := re.search(rf"static_assert!\({n} >= (-?\d+) && {n} <= (-?\d+)\)", body):
            found[name] = (int(m[1]), int(m[2]))
        elif m := re.search(rf"static_assert!\((-?\d+) <= {n} && {n} <= (-?\d+)\)", body):
            found[name] = (int(m[1]), int(m[2]))
        elif m := re.search(rf"static_assert!\({n} == (-?\d+)\)", body):
            found[name] = (int(m[1]), int(m[1]))
        else:
            return None
    return found


def collect(src):
    """The functions: name, unsafe, consts with ranges, parameters, result, features, gates;
    and the names left out, with why."""
    fns, skipped, seen, delegates = [], {}, set(), []
    for rel in SOURCES:
        text = open(os.path.join(src, rel)).read()
        for m in FN.finditer(text):
            unsafe, name, generics, params, ret = m[1], m[2], m[3], m[4], m[5]
            attrs = attributes(text, m.start())
            if any(a.startswith("#[cfg(") and ('target_arch = "arm"' in a and "not(" not in a
                                               or 'target_endian = "big"' in a) for a in attrs):
                continue
            consts = [g.split(":")[0].replace("const", "").strip() for g in split_top(generics)]
            if not all(re.fullmatch(r"const \w+: i32", g.strip()) for g in split_top(generics)):
                skipped[name] = "generic over a type"
                continue
            if name in seen:
                continue
            features = set()
            for a in attrs:
                if "target_feature(enable" not in a:
                    continue
                if a.startswith("#[cfg_attr(") and ('target_arch = "arm"' in a and "not(target_arch" not in a):
                    continue
                for it in re.findall(r'target_feature\(enable = "([^"]*)"\)', a):
                    features.update(f.strip() for f in it.split(","))
            gates = {g for a in attrs for g in re.findall(r'unstable\(feature = "(\w+)"', a)
                     if not a.startswith("#[cfg_attr(") or "not(target_arch" in a or 'target_arch = "arm"' not in a}
            if unknown := features - DETECTED:
                skipped[name] = f"needs {', '.join(sorted(unknown))}"
                continue
            body = body_of(text, m.end())
            got = ranges(consts, body)
            if got is None:
                # One that only passes its immediate on takes the range of the function it
                # calls, once that is known.
                callee = re.search(r"(\w+)::<(\w+)>\(", body)
                if callee and callee[2] in consts and len(consts) == 1:
                    delegates.append((callee[1], dict(name=name, unsafe=bool(unsafe), consts=consts,
                                                      args=[], ret="", features=[], gates=set()), params, ret, features, gates))
                    seen.add(name)
                    continue
                skipped[name] = "no static_assert gives its immediate's range"
                continue
            seen.add(name)
            args, types = [], {}
            for p in split_top(params):
                pname, ty = p.split(":", 1)
                args.append(ty.strip())
                types[pname.strip()] = ty.strip()
            # The lane an immediate picks is one of the vector's, whatever bits the assert
            # allows: `vgetq_lane_u64` asserts two bits of a vector of two lanes.
            for vector, const in re.findall(r"simd_(?:extract|insert)!\(\s*(\w+),\s*(\w+)", body):
                lanes = re.fullmatch(r"\w+?\d+x(\d+)_t", types.get(vector, ""))
                if lanes and const in got:
                    lo, hi = got[const]
                    got[const] = (lo, min(hi, int(lanes[1]) - 1))
            # A fixed-point conversion has at most as many fraction bits as its integer:
            # `vcvts_n_f32_s32` asserts 1 to 64, and LLVM selects nothing past 32.
            if (fixed := re.fullmatch(r"vcvt\w*_n_\w*?[su](16|32|64)\w*", name)) and len(consts) == 1:
                lo, hi = got[consts[0]]
                got[consts[0]] = (lo, min(hi, int(fixed[1])))
            fns.append(dict(name=name, unsafe=bool(unsafe), consts=consts, ranges=got, args=args,
                            ret=(ret or "").strip(), features=sorted(features | {"neon"}), gates=gates,
                            passes=re.findall(r"(\w+)::<(\w+)>\(", body)))
    known = {fn["name"]: fn for fn in fns}
    for callee, fn, params, ret, features, gates in delegates:
        if callee not in known:
            skipped[fn["name"]] = "no static_assert gives its immediate's range"
            continue
        fn.update(ranges={fn["consts"][0]: next(iter(known[callee]["ranges"].values()))},
                  args=[p.split(":", 1)[1].strip() for p in split_top(params)], ret=(ret or "").strip(),
                  features=sorted(features | {"neon"}), gates=gates)
        fns.append(fn)
    # A function that passes its immediate on is called only with what the callee takes too:
    # `vduph_laneq_f16` asserts four bits and calls `vgetq_lane_f16`, which takes three.
    known = {fn["name"]: fn for fn in fns}
    changed = True
    while changed:
        changed = False
        for fn in fns:
            for callee, const in fn.get("passes", []):
                if callee not in known or const not in fn["ranges"] or len(known[callee]["consts"]) != 1:
                    continue
                (lo, hi), (clo, chi) = fn["ranges"][const], next(iter(known[callee]["ranges"].values()))
                narrowed = (max(lo, clo), min(hi, chi))
                if narrowed != (lo, hi):
                    fn["ranges"][const] = narrowed
                    changed = True
    return fns, skipped


def values(lo, hi):
    out = [lo, (lo + hi) // 2, hi]
    return sorted(set(out))


def instantiations(fn):
    """The const arguments to call `fn` with: every const at its least, middle and greatest
    value together, so each end of each range is called."""
    per = [values(*fn["ranges"][c]) for c in fn["consts"]]
    count = max(len(v) for v in per)
    return [[v[min(i, len(v) - 1)] for v in per] for i in range(count)]


# `core::arch::aarch64` names the scalars of its polynomial vectors but does not export them.
POLY = {"p8": "u8", "p16": "u16", "p64": "u64", "p128": "u128"}


def reader(ty, k):
    ty = re.sub(r"\bp(8|16|64|128)\b", lambda m: POLY[m[0]], ty)
    if ty.startswith("*const "):
        return f"i.at::<{ty[7:]}>({k})"
    if ty.startswith("*mut "):
        return f"o.at::<{ty[5:]}>({k})"
    if ty.startswith(("float64", "f64")):
        return f"i.f64s::<{ty}>({k})"
    if ty.startswith(("float32", "f32")):
        return f"i.f32s::<{ty}>({k})"
    if ty.startswith(("float16", "f16")):
        return f"i.f16s::<{ty}>({k})"
    return f"i.bytes::<{ty}>({k})"


HEADER = '''\
//! Generated by `harness/neon_ops.py` from the pinned toolchain's `core::arch::aarch64`: each
//! function of NEON with a const parameter, called at the least, middle and greatest value of
//! its immediate, with inputs of edge values, each result hashed. Being generic, these are
//! compiled by the crate that calls them, not by a build of the standard library. A machine
//! without an extension prints that it has not.

use core::arch::aarch64::*;
use core::mem::size_of;

/// Inputs: bytes, and words of floats with edge values among them, read as any type.
pub struct In {
    bytes: Vec<u8>,
    f16s: Vec<u16>,
    f32s: Vec<f32>,
    f64s: Vec<f64>,
}

impl In {
    fn new() -> In {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut bytes: Vec<u8> = (0..8192).map(|_| next() as u8).collect();
        for (at, edge) in [0u8, 0xff, 0x80, 0x7f, 1].iter().enumerate() {
            for it in (at * 13..8192).step_by(61) {
                bytes[it] = *edge;
            }
        }
        let floats =
            [0.0, -0.0, 1.0, -1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e-310, 3.0e38, 0.5];
        let mut f64s: Vec<f64> = (0..2048).map(|_| (next() as i64 as f64) / 1e15).collect();
        let mut f32s: Vec<f32> = (0..4096).map(|_| (next() as i32 as f32) / 1e6).collect();
        for at in (0..2048).step_by(3) {
            f64s[at] = floats[at % floats.len()];
        }
        for at in (0..4096).step_by(3) {
            f32s[at] = floats[at % floats.len()] as f32;
        }
        // Halves of magnitudes about 2^-6 to 2^9, and zeros, ones, NaNs, infinities and
        // subnormals among them.
        let halves = [0x0000u16, 0x8000, 0x3c00, 0xbe00, 0x7e00, 0x7c00, 0xfc00, 0x0001, 0x7bff, 0x3800];
        let mut f16s: Vec<u16> = (0..8192).map(|_| (next() as u16 & 0x83ff) | 0x2400 + (next() as u16 % 16) * 0x400).collect();
        for at in (0..8192).step_by(3) {
            f16s[at] = halves[at % halves.len()];
        }
        In { bytes, f16s, f32s, f64s }
    }

    /// The address of the `k`th 64 bytes of the bytes, as a `T`.
    fn at<T>(&self, k: usize) -> *const T {
        self.bytes[k * 64..].as_ptr().cast()
    }

    fn bytes<T>(&self, k: usize) -> T {
        const { assert!(size_of::<T>() <= 64) };
        // SAFETY: any bits are a NEON vector or a scalar it holds.
        unsafe { core::ptr::read_unaligned(self.at::<T>(k)) }
    }

    fn f16s<T>(&self, k: usize) -> T {
        // SAFETY: as above; 64 bytes from `k * 32` halves on are in the inputs.
        unsafe { core::ptr::read_unaligned(self.f16s[k * 32..].as_ptr().cast()) }
    }

    fn f32s<T>(&self, k: usize) -> T {
        // SAFETY: as above.
        unsafe { core::ptr::read_unaligned(self.f32s[k * 16..].as_ptr().cast()) }
    }

    fn f64s<T>(&self, k: usize) -> T {
        // SAFETY: as above.
        unsafe { core::ptr::read_unaligned(self.f64s[k * 8..].as_ptr().cast()) }
    }
}

/// The hash of each result so far, and memory stores write to.
pub struct Out {
    text: String,
    memory: Vec<u8>,
}

impl Out {
    fn at<T>(&mut self, k: usize) -> *mut T {
        self.memory[k * 64..].as_mut_ptr().cast()
    }

    fn v<T: Copy>(&mut self, value: T) {
        // SAFETY: every byte of a NEON vector, a tuple of them or a scalar is initialised.
        let bytes = unsafe { core::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) };
        let mut hash = 0x811c_9dc5u32;
        for byte in bytes {
            hash = (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
        }
        self.text.push_str(&format!("{:04x}", hash as u16 ^ (hash >> 16) as u16));
    }

    /// What the stores wrote, then the memory as before.
    fn written(&mut self) {
        let memory = core::mem::replace(&mut self.memory, vec![0x5a; 1024]);
        self.v(memory.iter().fold(0u64, |h, b| h.rotate_left(5) ^ u64::from(*b)));
    }
}
'''


def generate(src):
    fns, skipped = collect(src)
    groups = {}
    for fn in fns:
        groups.setdefault(tuple(fn["features"]), []).append(fn)
    out = [HEADER]
    runs = []
    for features, members in sorted(groups.items()):
        calls = []
        for fn in members:
            mark = "!" if fn["unsafe"] else ""
            for at, consts in enumerate(instantiations(fn)):
                args = ", ".join(reader(ty, (at + n) % 8) for n, ty in enumerate(fn["args"]))
                turbofish = f"::<{', '.join(str(c) for c in consts)}>"
                call = f"{fn['name']}{turbofish}({args})"
                stores = any(ty.startswith("*mut ") for ty in fn["args"])
                if fn["ret"] and fn["ret"] != "()":
                    calls.append(f"{mark}o.v({call});")
                else:
                    calls.append(f"{mark}{call};")
                if stores:
                    calls.append("o.written();")
        enable = ",".join(features)
        for start in range(0, len(calls), CALLS_PER_GROUP):
            index = len(runs)
            chunk = calls[start:start + CALLS_PER_GROUP]
            out.append(f'#[target_feature(enable = "{enable}")]\nunsafe fn group{index}(i: &In, o: &mut Out) {{')
            if any(c.startswith("!") for c in chunk):
                out.append("    // SAFETY: each pointer is to 64 bytes of the inputs or of `o.memory`.\n    unsafe {")
                out.extend(f"        {c.lstrip('!')}" for c in chunk)
                out.append("    }")
            else:
                out.extend(f"    {c}" for c in chunk)
            out.append("}")
            runs.append((index, features))
    out.append("pub fn run() -> String {\n    let i = In::new();\n    let mut o = Out { text: String::new(), memory: vec![0x5a; 1024] };")
    for index, features in runs:
        detect = " && ".join(f'std::arch::is_aarch64_feature_detected!("{f}")' for f in features)
        out.append(f"    if {detect} {{\n        // SAFETY: the machine has {', '.join(features)}.\n"
                   f"        unsafe {{ group{index}(&i, &mut o) }}\n    }} else {{\n"
                   f"        o.text.push_str(\"no {'+'.join(features)}\");\n    }}\n    o.text.push('\\n');")
    out.append("    o.text\n}\n")
    gates = sorted({g for fn in fns for g in fn["gates"]} - {"stdarch_arm_neon_intrinsics"})
    detection = sorted({f for fn in fns for f in fn["features"]} & DETECTION_GATES)
    return "\n".join(out), fns, skipped, gates, detection


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--src", help="core_arch's src directory (default: the pinned toolchain's)")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    text, fns, skipped, gates, detection = generate(args.src or sysroot())
    calls = sum(len(instantiations(fn)) for fn in fns)
    print(f"neon_ops: {len(fns)} functions, {calls} calls; {len(skipped)} left out", file=sys.stderr)
    reasons = {}
    for name, why in skipped.items():
        reasons.setdefault(why, []).append(name)
    for why, names in sorted(reasons.items()):
        print(f"  {len(names)} {why}: {', '.join(sorted(names)[:6])}{' ...' if len(names) > 6 else ''}", file=sys.stderr)
    print(f"  gates: {', '.join(gates)}; detection of {', '.join(detection)}", file=sys.stderr)
    if args.check:
        if open(OUT).read() != text:
            sys.exit(f"error: {OUT} is not what neon_ops.py writes: run it")
        return
    with open(OUT, "w") as f:
        f.write(text)


if __name__ == "__main__":
    main()
