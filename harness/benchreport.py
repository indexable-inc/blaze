#!/usr/bin/env python3
"""benchreport.py: docs/benchmarks.md's tables and the README's numbers, from results of bench.py.

    benchreport.py RESULT [--name NAME] [--stored FILE] [--noise PERCENT] [--max-age HOURS] [--force] [--check]
    benchreport.py --render
    benchreport.py --charts

Every table is a matrix over the platforms of benchplatforms.py (macOS arm64, Linux aarch64,
Linux x86-64), each measured on a machine of its own. RESULT is what `bench.py --out` wrote on
one of them; its platform is its host's. It is compared with that platform's stored result
(docs/benchmarks/<platform>.json, e.g. linux-x86-64.json; `--stored` to say otherwise). When the
two differ by more than noise, the stored one is replaced and every table it feeds is written
again from the newest stored result of each platform: a platform that did not run keeps its last
numbers, under its machine, date and commit. Otherwise nothing is written. More than noise is
any of:
  - a median time of either side, in any phase, moved by more than `--noise` percent (default
    10) and by more than 50 ms
  - a phase the daemon built is now built by cargo, or the other way round
  - a project or backend is new or gone
  - a peak of either side moved by more than noise and 100 MB
  - the stored result is older than `--max-age` hours (default 24), so that the commit the
    tables name is never more than a day behind main
  - `--force`
What a result of bench.py writes: in the README, its opening numbers (the edits chart's first
row, in words) between `<!-- benchheadline:start -->` and `<!-- benchheadline:end -->`, and a
small table of a few projects on one platform (PICK) between `<!-- benchpick:start -->` and
`<!-- benchpick:end -->`; in docs/benchmarks.md, the build table between
`<!-- benchsummary:start -->` and `<!-- benchsummary:end -->`, memory and disk (bench.py's
`memory`, `resident` and `disk_mb`, docs/benchmarks.md "Memory and disk") between
`<!-- benchmem:start -->` and `<!-- benchmem:end -->`, and every number between
`<!-- bench:start -->` and `<!-- bench:end -->`.
A runtime result (v1's harness/runbench.py's numbers; v2 has no runtime bench yet) writes docs/benchmarks.md's
runtime table instead, between `<!-- runbench:start -->` and `<!-- runbench:end -->`: how fast each
benchmark's program runs built by blaze's LLVM against `cargo build --release`, and the
geometric mean. It is stored in docs/benchmarks/runtime-<platform>.json, and differs when a
median of any build moved by more than noise, a benchmark is new or gone, or the stored run is
older than `--max-age`. A build that failed, or whose program's run failed, has the whole result
refused.
`--name NAME` is a project measured on its own (bun, by the bench-bun workflow, which needs hours):
its results are stored in docs/benchmarks/NAME-<platform>.json and runtime-NAME-<platform>.json,
compared with those, and are a project like the others in every table and chart: each platform's
results are its own result's with those of each project measured on its own folded in
(`stored_all`), and a footnote under each table names the machine, commit and date of each such
project. docs/benchmarks.md has every number of each, under its platform.
A result in which a program said something else than blaze's cold build of it, or its run
failed (`wrong`), is refused: exit 2,
nothing written. A test suite that ended otherwise under `blaze test` is not refused: the
tests table says how, and the workflow fails after writing. `--check` writes nothing and exits 1
when it would have written. `--memory`, from before there were platforms, is the same as no flag:
a platform's memory is its result's.

Every summary table comes with its chart (benchcharts.py), written to
docs/benchmarks/charts/ by the same call that writes the table: the edit speed-ups with the
build table, memory and disk with the memory table, the runtime ratios with the runtime table,
each blaze against cargo with a bar per platform and each platform's geometric mean. `--render` writes every table and chart again from the stored results, and
`--charts` the charts alone.

The last line says `changed` or `unchanged`, and so does `changed=true|false` in
$GITHUB_OUTPUT when that is set.
"""
import datetime, json, math, os, re, shutil, sys

import benchcharts
from benchplatforms import NAMES, platform, slug

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BENCH_DIR = os.path.join(ROOT, "docs/benchmarks")
README = os.path.join(ROOT, "README.md")
DOC = os.path.join(ROOT, "docs/benchmarks.md")
START, END = "<!-- bench:start -->", "<!-- bench:end -->"
RUN_START, RUN_END = "<!-- runbench:start -->", "<!-- runbench:end -->"
MEM_START, MEM_END = "<!-- benchmem:start -->", "<!-- benchmem:end -->"
HEADLINE_START, HEADLINE_END = "<!-- benchheadline:start -->", "<!-- benchheadline:end -->"
SUM_START, SUM_END = "<!-- benchsummary:start -->", "<!-- benchsummary:end -->"
PICK_START, PICK_END = "<!-- benchpick:start -->", "<!-- benchpick:end -->"
# The README's small table: an edit to one function of these projects, as the README names them, on
# PICK_ON, the platform that measures every project.
PICK = {"ripgrep": "ripgrep", "nushell": "nushell", "helix": "helix", "bun": "bun (1.25M lines of Rust)"}
PICK_ON = "Linux x86-64"
# Where a platform's result was stored before there were platforms: read when the new one is missing.
LEGACY = {"linux-x86-64.json": "linux-x86_64.json", "macos-arm64.json": "memory-macos-arm64.json"}
FLOOR_MB = 100
REPO = "https://github.com/indexable-inc/blaze"
FLOOR_S = 0.05

# The order of the projects and phases in the tables, and what the README calls each edit.
PROJECTS = ["ripgrep", "fd", "bat", "hyperfine", "just", "axum", "nushell", "helix", "bevy", "biome", "zed", "cargo", "bun"]
# The projects measured on their own (`--name`): what to say of each under the tables.
APART = {
    "bun": "It is the Rust port of [bun](https://github.com/oven-sh/bun), 1.25M lines of Rust in 103 crates on a nightly "
           "beside a JavaScript engine's C and C++, built with frame pointers as bun's own build is "
           "(`-C force-frame-pointers=yes`; [what it took](docs/research/bun.md)). Its test suites are not measured yet.",
}
EDITS = {
    "body": "Edit a function",
    "leaf": "Edit a widely used function",
    "signature": "Change a signature",
    "generic": "Edit a generic function",
    "newfn": "Add a function",
    "struct": "Add a struct field",
    "no-op": "Nothing changed",
}
DOC_PHASES = ["body", "leaf", "signature", "generic", "newfn", "struct", "no-op"]
# What docs/benchmarks.md calls each setup of bench.py, in the order of its table.
SETUPS = {
    "check": "`cargo check`",
    "mold": "mold",
    "wild": "wild",
    "threads8": "`-Zthreads=8`",
    "nightly": "nightly",
    "cranelift": "nightly, Cranelift",
    "fastest": "nightly, Cranelift, wild, `-Zthreads=8`",
}
README_PHASES = ["body", "leaf", "signature", "no-op"]
BACKEND_NAMES = {"cranelift": "Cranelift", "llvm": "LLVM"}
NOT_MEASURED = "not measured yet"


def seconds(value):
    if value is None:
        return "-"
    return f"{value * 1000:.0f} ms" if value < 1 else f"{value:.2f} s"


def speedup(cargo, ours):
    if not cargo or not ours:
        return ""
    ratio = cargo / ours
    return f"{ratio:.0f}x" if ratio >= 10 else f"{ratio:.1f}x"


def spread(samples):
    if not samples:
        return ""
    return f"{seconds(min(samples))} to {seconds(max(samples))}"


def rank(key):
    project = key.split("/")[0]
    return (PROJECTS.index(project) if project in PROJECTS else len(PROJECTS), key)


def ordered(result, backend):
    return sorted((k for k in result["results"] if k.endswith("/" + backend)), key=rank)


def project_name(key, measured):
    tag = re.sub(r"^[a-z-]*?v?(?=\d)", "", measured.get("tag", ""))
    return f"{key.split('/')[0]} {tag}".strip()


def failure(key, measured):
    """What failed and where, in the project's own paths: a line for under the tables."""
    said = [line for line in measured["failed"].splitlines()[1:] if line.strip()] or measured["failed"].splitlines()
    said = re.sub(r"/\S*?/[\w.-]+-(cranelift|llvm)/(target/\S*?: )?", "", said[-1].removeprefix("error: "))
    said = said.removeprefix("blaze did not build ")
    if len(said) > 140:
        said = said[:140].rsplit(" ", 1)[0]
        said += "`" * (said.count("`") % 2) + " …"
    return f"{project_name(key, measured)}: only the steps shown are measured, then the build failed: {said}"


def failures(result, keys):
    failed = [failure(k, result["results"][k]) for k in keys if result["results"][k].get("failed")]
    return ["", *(f"- {line}" for line in failed)] if failed else []


def cargo_built(phase):
    return phase is not None and phase.get("built") != "daemon"


def short(commit):
    return (commit or "unknown")[:10]


def commit_link(result):
    commit = result.get("commit")
    return f"[`{short(commit)}`]({REPO}/commit/{commit})"


def machine_line(result):
    host = result.get("host") or {}
    cores, of = host.get("cores") or result.get("cores"), host.get("machine_cores")
    cpu = re.sub(r"( \d+-Core)? Processor$", "", host.get("cpu") or "") or None
    details = [p for p in (cpu, f"{cores} of its {of} cores" if of and of != cores else f"{cores} cores",
                           f"{host['memory_gib']} GiB" if host.get("memory_gib") else None) if p]
    return f"{host.get('name') or 'one machine'} ({', '.join(details)})"


def loaded(result):
    """The load average at the start, when it says the machine was busy with other work."""
    host = result.get("host") or {}
    load = float((host.get("load") or "0").split(", ")[0])
    cores = host.get("cores") or result.get("cores") or 1
    return f", load average {load:.0f} at the start" if load >= max(1, cores / 2) else ""


def megabytes(mb):
    if mb is None:
        return "-"
    return f"{mb / 1000:.1f} GB" if mb >= 1000 else f"{mb:,.0f} MB"


def has_memory(result):
    return any(phase.get("memory") for measured in result["results"].values() for phase in measured["phases"].values())


def stored_path(platform_name, kind="", name=None):
    """docs/benchmarks/[runtime-][NAME-]<platform>.json"""
    return os.path.join(BENCH_DIR, "-".join(p for p in (kind, name, slug(platform_name)) if p) + ".json")


def load_stored(path):
    for candidate in (path, os.path.join(os.path.dirname(path), LEGACY.get(os.path.basename(path), ""))):
        if os.path.isfile(candidate):
            return json.load(open(candidate))
    return None


def stored_all(kind="", override=None, name=None):
    """The newest stored result of each platform that has one, with the newest of each project
    measured on its own (`names()`) folded in (`fold`). `override`, a result just measured (of
    project `name`, or the platform's own), takes the place of the stored one."""
    found = {}
    for p in NAMES:
        own = load_stored(stored_path(p, kind))
        apart = {n: r for n in names() if (r := load_stored(stored_path(p, kind, n)))}
        if override and platform(override) == p:
            if name:
                apart[name] = override
            else:
                own = override
        if own or apart:
            found[p] = fold(own, apart, kind)
    return found


def fold(own, apart, kind):
    """One platform's result with the projects (for the runtime, the programs) of each result in
    `apart` (name: result) added to its own, and its geometric means taken again over all of them.
    The results it came from stay as `own` and `apart`, for the lines that name their machines."""
    if not apart:
        return own
    merged = dict(own or next(iter(apart.values())), own=own, apart=apart)
    parts = ([own] if own else []) + list(apart.values())
    if kind == "runtime":
        runs = [r.get("runbench") or {} for r in parts]
        programs = {p: v for rb in runs for p, v in (rb.get("programs") or {}).items()}
        builds = list(dict.fromkeys(b for rb in runs for b in rb.get("builds") or []))
        means = {b: math.exp(sum(map(math.log, found)) / len(found)) for b in builds
                 if (found := [row[b]["ratio"] for v in programs.values() for row in v["benches"].values() if row.get(b, {}).get("ratio")])}
        variants = {k: v for rb in runs for k, v in (rb.get("variants") or {}).items()}
        merged["runbench"] = dict(runs[0], programs=programs, builds=builds, variants=variants, geomean=means)
    else:
        merged["results"] = {k: m for r in parts for k, m in r["results"].items()}
    return merged


def own(result):
    """What a platform measured in its own run, without the projects measured on their own: None
    when it has only those."""
    return result.get("own") if "apart" in result else result


def who(result):
    return f"[by CI]({result['run_url']})" if result.get("run_url") else "by hand"


def machines(results, extra=lambda r: "", said=False):
    """A line per platform: its machine, the blaze it measured and when, or that it has not
    been measured; then a line for each project measured on its own, with its machines (and what
    it is, when `said`)."""
    lines = []
    for p in NAMES:
        r = own(results[p]) if p in results else None
        lines.append(f"- **{p}**: " + (f"{machine_line(r)}, blaze {commit_link(r)} on {r['date'][:10]}{extra(r)}{loaded(r)}, {who(r)}."
                                       if r else f"{NOT_MEASURED} ([the machines](docs/benchmarks.md#the-machines))."))
    return lines + apart_lines(results, extra, said)


def apart_lines(results, extra=lambda r: "", said=False):
    """For each project measured on its own, where and when: `said` adds what it is (APART)."""
    lines = []
    for name in names():
        on = {p: r["apart"][name] for p, r in results.items() if name in r.get("apart", {})}
        if on:
            what = f" {APART[name]}" if said and name in APART else ""
            lines.append(f"- **{name}** is measured on its own, by the `bench-{name}` workflow: " + "; ".join(
                f"{p} on {machine_line(r)}, blaze {commit_link(r)} on {r['date'][:10]}{extra(r)}{loaded(r)}, {who(r)}"
                for p, r in on.items()) + "." + what)
    return lines


def where(results):
    """Each platform's machine (the start of its name) and the day it was measured, for a chart's
    footer, then each project measured on its own; the commit is in the table's machine lines."""
    def name(r):
        host = r.get("host") or {}
        named = re.split(r" \(|, ", host.get("name") or "")[0]
        return named or re.sub(r"( \d+-Core)? Processor$", "", host.get("cpu") or "") or "one machine"
    said = [f"{p}: {name(r)}, {r['date'][:10]}." for p, full in results.items() if (r := own(full))]
    for n in names():
        on = {p: r["apart"][n] for p, r in results.items() if n in r.get("apart", {})}
        if on:
            said.append(f"{n}, measured on its own: " + "; ".join(f"{p}: {name(r)}, {r['date'][:10]}" for p, r in on.items()) + ".")
    return " ".join(said)


def keys_of(results, backend):
    return sorted({k for r in results.values() for k in r["results"] if k.endswith("/" + backend)}, key=rank)


def label_of(results, key):
    return next(project_name(key, r["results"][key]) for r in results.values() if key in r["results"])


def raw_links(results, kind=""):
    """Links to each platform's stored result, then to each project measured on its own's."""
    link = lambda p, name=None: f"[{p}]({os.path.relpath(stored_path(p, kind, name), ROOT)})"
    said = [", ".join(link(p) for p, r in results.items() if own(r))]
    for name in names():
        if on := [p for p, r in results.items() if name in r.get("apart", {})]:
            said.append(f"{name}: " + ", ".join(link(p, name) for p in on))
    return "; ".join(s for s in said if s)


def edit_cell(phase):
    if phase is None:
        return "", False
    fell_back = cargo_built(phase)
    return (f"{seconds(phase['cargo_s'])} → **{seconds(phase['wall_s'])}**{' (c)' if fell_back else ''} "
            f"({speedup(phase['cargo_s'], phase['wall_s'])})"), fell_back


def edit_speedups(results, backend):
    """(project, {platform: cargo's time ÷ blaze's for each kind of edit}), the edits chart's rows."""
    rows = []
    for key in keys_of(results, backend):
        by = {}
        for p, r in results.items():
            measured = r["results"].get(key)
            if measured:
                by[p] = [ph["cargo_s"] / ph["wall_s"] for label, ph in measured["phases"].items()
                         if label in EDITS and label != "no-op" and ph.get("cargo_s") and ph.get("wall_s")]
        rows.append((label_of(results, key), by))
    return rows


def edits_chart(results, backend="cranelift"):
    return benchcharts.edits(edit_speedups(results, backend), where(results))


def headline(results, backend="cranelift"):
    """The README's opening numbers: the edits chart's first row (each platform's geometric mean over
    the projects of the median over the kinds of edit), in words."""
    rows = edit_speedups(results, backend)
    said = []
    for p in NAMES:
        medians = [benchcharts.median(v) for _, by in rows if (v := by.get(p))]
        if medians:
            m = benchcharts.geomean(medians)
            factor = m if m >= 1 else 1 / m
            text = f"{factor:.0f}×" if factor >= 9.95 else f"{factor:.1f}×"
            said.append(f"**{text} {'faster' if m >= 1 else 'slower'}** on {p}")
    joined = ", ".join(said[:-1]) + f" and {said[-1]}" if len(said) > 1 else "".join(said)
    return HEADLINE_START + joined + HEADLINE_END


def edit_memory(measured):
    """The memory of the first edit a project measured it for (an edit to one function's body)."""
    phases = measured["phases"]
    return next((phases[l]["memory"] for l in DOC_PHASES if l != "no-op" and (phases.get(l) or {}).get("memory")), None)


def memory_backend(results):
    return next((b for b in BACKEND_NAMES if any(k.endswith("/" + b) and edit_memory(m) for r in results.values()
                                                 for k, m in r["results"].items())), None)


def memory_chart(results, backend):
    rows = []
    for key in keys_of(results, backend):
        by = {p: (m["cargo"]["peak_mb"], m["blaze"]["peak_mb"]) for p, r in results.items()
              if key in r["results"] and (m := edit_memory(r["results"][key]))}
        if by:
            rows.append((label_of(results, key), by))
    return benchcharts.memory(rows, where({p: r for p, r in results.items() if has_memory(r)}))


def on_disk(measured):
    """(cargo's target directory, blaze's target directory, store and home) in MB, once a
    project's edits are done, or None."""
    disk = measured.get("disk_mb") or {}
    ours = sum(disk.get(k) or 0 for k in ("target", "store", "home"))
    return (disk["cargo target"], ours) if disk.get("cargo target") and ours else None


def disk_chart(results, backend):
    rows = []
    for key in keys_of(results, backend):
        by = {p: d for p, r in results.items() if key in r["results"] and (d := on_disk(r["results"][key]))}
        if by:
            rows.append((label_of(results, key), by))
    return benchcharts.disk(rows, where({p: r for p, r in results.items()
                                         if any(on_disk(m) for k, m in r["results"].items() if k.endswith("/" + backend))}))


def pick_section(results, backend="cranelift"):
    """The README's small table: an edit to one function of each project of PICK on PICK_ON."""
    lines = [PICK_START, f"| Edit one function, then build ({PICK_ON}) | `cargo build` | `blaze build` |", "|---|---:|---:|"]
    measured = (results.get(PICK_ON) or {"results": {}})["results"]
    for key in keys_of(results, backend):
        name = key.split("/")[0]
        phase = (measured.get(key) or {"phases": {}})["phases"].get("body")
        if name in PICK and phase and phase.get("cargo_s") and phase.get("wall_s") and not cargo_built(phase):
            lines.append(f"| {PICK[name]} | {seconds(phase['cargo_s'])} | "
                         f"**{seconds(phase['wall_s'])}** ({speedup(phase['cargo_s'], phase['wall_s']).replace('x', '×')} faster) |")
    lines.append(PICK_END)
    return "\n".join(lines)


def for_docs(section):
    """A section whose links are written from the repository's root, with them made relative to
    docs/, for docs/benchmarks.md."""
    section = section.replace("[docs/benchmarks.md](docs/benchmarks.md)", "[every number below](#results)")
    section = re.sub(r'(\]\(|src="|srcset=")docs/benchmarks\.md(#[^)"]*)?', lambda m: m[1] + (m[2] or "#results"), section)
    return re.sub(r'(\]\(|src="|srcset=")docs/', r"\1", section)


def summary_section(results):
    """docs/benchmarks.md's build table: an edit to one function of each project on each platform,
    then every kind of edit by platform."""
    backend = "cranelift"
    lines = [SUM_START]
    if summary := edits_chart(results, backend):
        lines += [benchcharts.picture("edits", f"How much faster an edit builds under blaze than under cargo, by platform: {summary}"), ""]
    lines += ["Edit, then build, on real projects at pinned commits: incremental `cargo build` → `blaze build`, both "
              "warm, same linker, Cranelift, the median of each run's edits. Each platform is a machine of its "
              "own: compare numbers within a platform, not across.", ""]
    fell_back = False

    def phase_table(result, keys):
        nonlocal fell_back
        out = ["| Project | " + " | ".join(EDITS[p] for p in README_PHASES) + " |", "|---|" + "---:|" * len(README_PHASES)]
        for key in keys:
            measured = result["results"][key]
            cells = []
            for label in README_PHASES:
                text, fell = edit_cell(measured["phases"].get(label))
                fell_back |= fell
                cells.append(text)
            out.append(f"| {project_name(key, measured)} | " + " | ".join(cells) + " |")
        return out

    lines += ["An edit to one function, cargo → blaze:", "",
              "| Project | " + " | ".join(NAMES) + " |", "|---|" + "---:|" * len(NAMES)]
    for key in keys_of(results, backend):
        cells = []
        for p in NAMES:
            measured = (results.get(p) or {"results": {}})["results"].get(key)
            text, fell = edit_cell(measured["phases"].get("body")) if measured else ("-", False)
            fell_back |= fell
            cells.append(text or "-")
        lines.append(f"| {label_of(results, key)} | " + " | ".join(cells) + " |")
    lines += ["", *machines(results, lambda r: f", median of {r['runs']} edits", said=True), "",
              "<details><summary>Every kind of edit, by platform</summary>"]
    for p, r in results.items():
        keys = ordered(r, backend)
        lines += ["", f"**{p}**", "", *phase_table(r, keys), *failures(r, keys)]
    lines += ["", "</details>", ""]
    if fell_back:
        lines += ["(c): the daemon handed this edit to cargo (a fallback, so about cargo's time).", ""]
    built = (f" The blaze measured is `cargo build --release`; the binaries a release ships are the `dist` profile with PGO, "
             f"which start and edit faster ([how]({REPO}/blob/main/docs/reports/RELEASE-BUILD.md)).")
    lines.append("Measured by hand with `harness/bench.py`, or by the `bench-readme` workflow (a project measured on its own, by "
                 "its own workflow) on each platform's benchmark machine once it is registered." + built + " Cold builds, the "
                 "spread of every number, the LLVM tier and how fast the built programs run: [docs/benchmarks.md](docs/benchmarks.md)."
                 + f" Raw JSON: {raw_links(results)}.")
    lines.append(SUM_END)
    return "\n".join(lines)


def memory_section(results):
    """docs/benchmarks.md's memory table: what an edit held at its peak on each platform, then by platform
    what the daemon holds between edits, what a cold build held and what each side keeps on disk."""
    measured = {p: r for p, r in results.items() if has_memory(r)}
    backend = memory_backend(measured)
    lines = [MEM_START]
    if backend is None:
        return "\n".join([MEM_START, "Memory: no platform's stored run measured it yet.", MEM_END])
    if summary := memory_chart(measured, backend):
        lines += [benchcharts.picture("memory", f"Peak memory while an edit to one function builds, blaze against cargo, by platform: {summary}"), ""]
    if summary := disk_chart(measured, backend):
        lines += [benchcharts.picture("disk", f"What a project's build keeps on disk, blaze against cargo, by platform: {summary}"), ""]
    lines += [f"Memory and disk, {BACKEND_NAMES[backend]}, from the same runs as the build table. Peak: the most every process "
              "doing the work held at once, cargo → blaze. For blaze that is the daemon, its workers and what they run, and "
              "`blaze build` with any cargo it ran; for plain cargo, cargo itself, rustc, build scripts and the linker. "
              "How it is measured: [docs/benchmarks.md](docs/benchmarks.md#memory-and-disk).", "",
              "Peak while an edit to one function builds, cargo → blaze:", "",
              "| Project | " + " | ".join(NAMES) + " |", "|---|" + "---:|" * len(NAMES)]
    pair = lambda m: f"{megabytes(m['cargo']['peak_mb'])} → **{megabytes(m['blaze']['peak_mb'])}**" if m else "-"
    keys = [k for k in keys_of(measured, backend) if any(k in r["results"] and edit_memory(r["results"][k]) for r in measured.values())]
    for key in keys:
        cells = [pair(edit_memory(measured[p]["results"][key])) if p in measured and key in measured[p]["results"] else "-" for p in NAMES]
        lines.append(f"| {label_of(measured, key)} | " + " | ".join(cells) + " |")
    if missing := [p for p in NAMES if p in results and p not in measured]:
        lines += ["", f"{', '.join(missing)}: its stored run is from before memory was measured."]
    lines += ["", "<details><summary>The daemon between edits, cold builds and disk, by platform</summary>"]
    for p, r in measured.items():
        lines += ["", f"**{p}**", "",
                  "| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |",
                  "|---|---:|---:|---:|---:|"]
        for key in [k for k in ordered(r, backend) if any(ph.get("memory") for ph in r["results"][k]["phases"].values())]:
            m = r["results"][key]
            cold = (m["phases"].get("cold") or {}).get("memory")
            resident = m.get("resident") or {}
            held = (f"{megabytes(resident['ready_mb'])} ready, {megabytes(resident['end_mb'])} after {resident['builds']} builds"
                    if resident.get("end_mb") is not None else megabytes(resident.get("ready_mb")) if resident else "")
            disk = m.get("disk_mb") or {}
            ours = sum(disk.get(k) or 0 for k in ("target", "store", "home"))
            on_disk = f"{megabytes(disk.get('cargo target'))} → {megabytes(ours)}" if disk else ""
            lines.append(f"| {project_name(key, m)} | {pair(edit_memory(m))} | {held} | {pair(cold) if cold else ''} | {on_disk} |")
    lines += ["", "On disk: cargo's target directory → blaze's target directory, store and home.", "", "</details>", MEM_END]
    return "\n".join(lines)


def memory_tables(result, keys, backend, name=""):
    """docs/benchmarks.md's memory tables of one backend: every step's peaks, the daemon between
    builds, and the disk."""
    keys = [k for k in keys if any(p.get("memory") for p in result["results"][k]["phases"].values())]
    if not keys:
        return []
    lines = ["", f"#### Memory and disk, {BACKEND_NAMES[backend]}{', ' + name if name else ''}", "",
             "Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest "
             "process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel "
             "kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint "
             "of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and "
             "what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts "
             "and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).", "",
             "| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |",
             "|---|---|---:|---:|---:|---:|---:|"]
    for key in keys:
        measured = result["results"][key]
        for label in ["cold", *DOC_PHASES]:
            m = (measured["phases"].get(label) or {}).get("memory")
            if not m:
                continue
            step = "Cold, until the daemon is ready" if label == "cold" else EDITS[label]
            lines.append(f"| {project_name(key, measured)} | {step} | {megabytes(m['cargo']['peak_mb'])} | {megabytes(m['blaze']['peak_mb'])} | "
                         f"{megabytes(m['cargo']['largest_mb'])} | {megabytes(m['blaze']['largest_mb'])} | {megabytes(m['blaze'].get('resident_mb'))} |")
    lines += ["", "The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). "
              "After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, "
              "over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.", "",
              "| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |",
              "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for key in keys:
        measured = result["results"][key]
        resident, disk = measured.get("resident") or {}, measured.get("disk_mb") or {}
        first = (resident.get("after") or [{}])[0]
        end = f"{megabytes(resident['end_mb'])} after {resident.get('builds')} builds" if resident.get("end_mb") is not None else "-"
        growth = (resident["end_mb"] - first["mb"]) if resident.get("end_mb") is not None and first.get("mb") is not None else None
        warm = (measured["phases"].get("cold") or {}).get("warm_store_memory")
        lines.append(f"| {project_name(key, measured)} | {megabytes(resident.get('ready_mb'))} | {megabytes(first.get('mb'))} | {end} | "
                     f"{'-' if growth is None else ('+' if growth >= 0 else '−') + megabytes(abs(growth))} | "
                     f"{megabytes(warm['peak_mb']) + ', ' + megabytes(warm['resident_mb']) if warm else '-'} | "
                     f"{megabytes(disk.get('cargo target'))} | {megabytes(disk.get('target'))} | {megabytes(disk.get('store'))} | {megabytes(disk.get('home'))} |")
    return lines



def setups_table(result, keys, backend, name=""):
    """The same steps built the fastest ways there are besides blaze, for the projects that had them."""
    keys = [k for k in keys if any(p.get("setups") for p in result["results"][k]["phases"].values())]
    if not keys:
        return []
    names = [s for s in SETUPS if any(s in (p.get("setups") or {}) for k in keys for p in result["results"][k]["phases"].values())]
    nightly = (result.get("host") or {}).get("nightly")
    lines = ["", f"#### Against the fastest setups, {BACKEND_NAMES[backend]}{', ' + name if name else ''}", "",
             "The same steps, each built after plain cargo by the fastest ways to build Rust there already are, each into a "
             "target directory of its own, median of the same runs. Plain cargo is incremental, with the default linker. "
             f"mold and wild link instead of GNU ld; `-Zthreads=8` is the parallel front end on the same compiler; the nightly ({nightly or 'see the JSON'}) "
             "is there to tell its own difference from Cranelift's (rustc_codegen_cranelift comes only with nightlies); `cargo check` "
             "builds no code, so nothing can be run after it. The fastest of each row is in bold; \"failed\" is a setup that could "
             "not build the project, and is not timed after that.", "",
             "| Project | Step | cargo | " + " | ".join(SETUPS[n] for n in names) + " | blaze |",
             "|---|---|---:|" + "---:|" * (len(names) + 1)]
    for key in keys:
        measured = result["results"][key]
        for label in ["cold", *DOC_PHASES]:
            phase = measured["phases"].get(label)
            if not phase or not phase.get("setups"):
                continue
            ours = phase["wall_s"] if label != "cold" else phase.get("wall_s")
            times = {"cargo": phase["cargo_s"], "blaze": ours}
            times.update({n: phase["setups"].get(n, {}).get("median_s") for n in names})
            # `cargo check` builds nothing to run, so it is not what the fastest is chosen from.
            fastest = min((v, n) for n, v in times.items() if v is not None and n != "check")[1]
            cell = lambda n: ("failed" if (phase["setups"].get(n) or {}).get("failed") else "-") if times[n] is None else (
                f"**{seconds(times[n])}**" if n == fastest else seconds(times[n]))
            step = "Cold, until the daemon is ready" if label == "cold" else EDITS[label]
            lines.append(f"| {project_name(key, measured)} | {step} | {cell('cargo')} | " + " | ".join(cell(n) for n in names) + f" | {cell('blaze')} |")
    failures = sorted({f"{project_name(k, result['results'][k])}, {SETUPS[n]}: {s['failed']}" for k in keys
                       for p in result["results"][k]["phases"].values() for n, s in (p.get("setups") or {}).items() if s.get("failed")})
    if failures:
        lines += [""] + [f"- {f}" for f in failures]
    return lines


def said_otherwise(difference):
    """What was wrong with a test suite under blaze, for a table cell."""
    return (difference or "").replace("|", "\\|")[:120]


def tests_table(result, keys, backend, name=""):
    keys = [k for k in keys if result["results"][k]["phases"].get("test")]
    if not keys:
        return []
    lines = ["", f"#### Test suites, {BACKEND_NAMES[backend]}{', ' + name if name else ''}", "",
             "`blaze test` and `cargo test` of the same tests, median of three: after the edit of a widely used function "
             "(the file named), and again with nothing changed. Each includes running the tests. First runs: the three untimed "
             "runs of `blaze test` before them (the first is handed to cargo, the daemon builds the test targets at the "
             "second). Every test has to pass under blaze by its own verdict, every time; cargo is only timed.", "",
             "| Project | Command | Tests | All pass | Edited | Edit, then test: cargo | blaze | | Built | Nothing changed: cargo | blaze | First runs of blaze |",
             "|---|---|---:|---|---|---:|---:|---:|---|---:|---:|---|"]
    for key in keys:
        test = result["results"][key]["phases"]["test"]
        if "edit_s" not in test:
            continue
        lines.append(f"| {project_name(key, result['results'][key])} | `cargo {' '.join(test['command'])}` | {test['tests']:,} | "
                     f"{'yes' if test['same'] else 'no: ' + said_otherwise(test.get('difference'))} | `{test['edited']}` | "
                     f"{seconds(test['edit_cargo_s'])} | {seconds(test['edit_s'])} | {speedup(test['edit_cargo_s'], test['edit_s'])} | {test['built'][:60]} | "
                     f"{seconds(test['cargo_s'])} | {seconds(test['wall_s'])} | {', '.join(seconds(s) for s in test['first_s'])} |")
    return lines



def runtime_wrong(result):
    """Why a runtime result may not be published: a build that failed or whose program
    did not run to the end."""
    wrong = []
    for name, program in (result.get("runbench") or {}).get("programs", {}).items():
        for build, info in program["builds"].items():
            if info.get("error"):
                wrong.append(f"{name} {build}: the build failed")
            for bench in info.get("failed_runs", []):
                wrong.append(f"{bench} {build}: the program's run failed")
        for bench, timed in program.get("benches", {}).items():
            for build in ("release", "blaze-engine"):
                if build not in timed:
                    wrong.append(f"{bench} {build}: not timed")
    return wrong


def times(ratio):
    return f"{ratio:.2f}x" if ratio < 10 else f"{ratio:.0f}x"



def runtime_differences(stored, result, noise, max_age_h):
    if stored is None or not stored.get("runbench"):
        return ["nothing stored"]
    why = []
    when = lambda r: datetime.datetime.strptime(r["date"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    if (when(result) - when(stored)).total_seconds() / 3600 > max_age_h:
        why.append("the stored result is old")
    rows = lambda r: {(p, b): row for p, prog in r["runbench"]["programs"].items() for b, row in prog["benches"].items()}
    was, now = rows(stored), rows(result)
    if set(was) != set(now):
        why.append(f"benchmarks: {sorted(was)} -> {sorted(now)}")
    for key, row in now.items():
        for build, cell in row.items():
            old = was.get(key, {}).get(build)
            if old and abs(cell["median_s"] - old["median_s"]) / old["median_s"] * 100 > noise:
                why.append(f"{key[1]} {build}: {old['median_s']} -> {cell['median_s']}")
    return why


def splice(path, section, start=START, end=END):
    text = open(path).read()
    pattern = re.compile(re.escape(start) + ".*?" + re.escape(end), re.S)
    if not pattern.search(text):
        raise SystemExit(f"{path} has no {start} ... {end} section")
    open(path, "w").write(pattern.sub(lambda _: section, text, count=1))


def differences(stored, result, noise, max_age_h):
    """Why `result` is worth writing over `stored`; empty when it is within noise."""
    if stored is None:
        return ["nothing stored"]
    why = []
    if set(stored["results"]) != set(result["results"]):
        why.append(f"projects: {sorted(stored['results'])} -> {sorted(result['results'])}")
    when = lambda r: datetime.datetime.strptime(r["date"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    age = (when(result) - when(stored)).total_seconds() / 3600
    if age > max_age_h:
        why.append(f"the stored result is {age:.0f} h old")
    for key, now in result["results"].items():
        was = stored["results"].get(key)
        if was is None:
            continue
        for label, phase in now["phases"].items():
            old = was["phases"].get(label)
            if old is None:
                why.append(f"{key} {label}: new")
                continue
            for field in ("wall_s", "cargo_s"):
                before, after = old.get(field), phase.get(field)
                if before and after and abs(after - before) > FLOOR_S and abs(after - before) / before * 100 > noise:
                    why.append(f"{key} {label} {field}: {before} -> {after}")
            if (old.get("built") == "daemon") != (phase.get("built") == "daemon"):
                why.append(f"{key} {label}: built by {old.get('built')} -> {phase.get('built')}")
            for side in ("blaze", "cargo"):
                before, after = ((p.get("memory") or {}).get(side, {}).get("peak_mb") for p in (old, phase))
                if (before is None) != (after is None):
                    why.append(f"{key} {label}: {side} memory {'measured' if after is not None else 'no longer measured'}")
                elif before and abs(after - before) > FLOOR_MB and abs(after - before) / before * 100 > noise:
                    why.append(f"{key} {label} {side} peak: {before} MB -> {after} MB")
    return why



def options(argv, name, default=None):
    if name in argv:
        at = argv.index(name)
        value = argv[at + 1]
        del argv[at:at + 2]
        return value
    return default


def doc_section(results):
    """docs/benchmarks.md's results: every number of the newest run of each platform."""
    lines = [START, "Every number of the newest run of each platform, then of each project measured on its own on it "
             "(bun, by the `bench-bun` workflow), under its own machine. Each platform is a machine of its own: its numbers "
             "compare with each other and with its own history, not with another platform's.", ""]
    for p, r in results.items():
        if own(r):
            lines += platform_doc(p, own(r), stored_path(p))
        for name, apart in r.get("apart", {}).items():
            lines += platform_doc(f"{p}, {name}", apart, stored_path(p, name=name))
    lines.append(END)
    return "\n".join(lines)


def platform_doc(name, result, stored):
    host = result.get("host") or {}
    raw = os.path.relpath(stored, os.path.join(ROOT, "docs"))
    lines = [f"### {name}", "",
             f"blaze {commit_link(result)}, {result['date']}, Rust {result['toolchain']}, {result['runs']} runs of each edit, "
             f"measured {who(result)}. Raw: [{raw}]({raw}).", "",
             "| Machine | |", "|---|---|"]
    shown = dict(host, os="macOS" if host.get("os") == "Darwin" else host.get("os"),
                 ld=None if "unknown option" in (host.get("ld") or "") else host.get("ld"))
    for label, field in (("Name", "name"), ("CPU", "cpu"), ("Cores", "cores"), ("Memory (GiB)", "memory_gib"), ("OS", "os"),
                         ("Kernel", "kernel"), ("rustc", "rustc"), ("C compiler", "cc"), ("Linker", "ld"), ("LLVM", "llvm"),
                         ("Incremental", "incremental"), ("Compiler cache", "wrapper"), ("Load average at the start", "load")):
        value = shown.get(field)
        if value is None and field in ("load", "os"):
            continue
        lines.append(f"| {label} | {value if value is not None else 'none' if field == 'wrapper' else '-'} |")
    lines.append("")
    for backend in [b for b in BACKEND_NAMES if any(k.endswith("/" + b) for k in result["results"])]:
        keys = ordered(result, backend)
        lines += [f"#### Edits, {BACKEND_NAMES[backend]}, {name}", "",
                  "Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). "
                  "Link: the change was spliced into the program, or it was linked again.", "",
                  "| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |",
                  "|---|---|---:|---:|---:|---|---|---|---|"]
        for key in keys:
            measured = result["results"][key]
            for label in DOC_PHASES:
                phase = measured["phases"].get(label)
                if phase is None:
                    continue
                lines.append(f"| {project_name(key, measured)} | {EDITS[label]} | {seconds(phase['cargo_s'])} | {seconds(phase['wall_s'])} | "
                             f"{speedup(phase['cargo_s'], phase['wall_s'])} | {spread(phase.get('cargo_samples_s'))} | {spread(phase.get('samples_s'))} | "
                             f"{phase['built'][:80]} | {phase.get('link') or ''} |")
        lines += failures(result, keys)
        cold_runs = result.get("cold_runs", 1)
        lines += ["", f"#### Cold, {BACKEND_NAMES[backend]}, {name}", "",
                  "From an empty target directory, crates already downloaded, " + ("one run." if cold_runs == 1 else
                  f"median of {cold_runs} runs, each from nothing.") + " The empty store fetches the standard library as a "
                  "release's first build does. `blaze build` is cargo's own build (the daemon is "
                  "not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. "
                  "Warm store: the same in another checkout of the same commit, with the store the first one filled.", "",
                  "| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | "
                  "Functions compiled | Daemon memory | Store |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for key in keys:
            measured = result["results"][key]
            cold = measured["phases"].get("cold")
            if cold:
                ready = lambda total, samples: seconds(total) + (f" ({spread(samples)})" if samples and len(samples) > 1 else "")
                store = f"{measured['store_mb']:,} MB" if measured.get("store_mb") is not None else "-"
                runs = measured.get("cold_runs", cold_runs)
                label = project_name(key, measured) + ("" if runs == cold_runs else " (one run)" if runs == 1 else f" ({runs} runs)")
                lines.append(f"| {label} | {ready(cold['cargo_s'], cold.get('cargo_samples_s'))} | "
                             f"{ready(cold.get('build_s'), cold.get('build_samples_s'))} | {ready(cold.get('wall_s'), cold.get('samples_s'))} | "
                             f"{ready(cold.get('warm_store_s'), cold.get('warm_store_samples_s'))} | "
                             f"{cold.get('compiled', 0):,} | {cold.get('rss_mb', 0):,} MB | {store} |")
        lines += memory_tables(result, keys, backend, name) + setups_table(result, keys, backend, name) + tests_table(result, keys, backend, name)
        timed = [k for k in keys if result["results"][k].get("runtime")]
        if timed:
            lines += ["", f"#### How fast the program runs, {BACKEND_NAMES[backend]}, {name}", "",
                      "The same command run by the program blaze linked and by cargo's debug and release builds, in turn. "
                      "Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran "
                      "(median of the runs, as the kernel kept it) and the size of the program.", "",
                      "| Project | Command | blaze | cargo debug | cargo release |", "|---|---|---:|---:|---:|"]
            for key in timed:
                run = result["results"][key]["runtime"]
                cell = lambda r: f"{seconds(r['median_s'])} ({seconds(r['min_s'])} to {seconds(r['max_s'])})" + (
                    f"; {megabytes(r['peak_mb'])}, {r['size_mb']} MB" if r.get("peak_mb") is not None else "")
                command = " ".join(run["command"]).replace("{library}", "<blaze's library/>").replace("|", "\\|")
                lines.append(f"| {project_name(key, result['results'][key])} | `{command}` | {cell(run['blaze'])} | "
                             f"{cell(run['cargo debug'])} | {cell(run['cargo release'])} |")
        lines.append("")
    return lines


ROUGH = 1.5


def rough(row):
    """Whether a benchmark's runs varied too much to read its ratio closely: the slowest run of
    release or of blaze took more than ROUGH times its fastest."""
    return any(max(runs) > ROUGH * min(runs) for b in ("release", "blaze-engine") if (runs := (row.get(b) or {}).get("runs")) and min(runs) > 0)


def ratio_cell(row):
    return ("≈" if rough(row) else "") + times(row["blaze"]["ratio"])


ROUGH_NOTE = f"≈: that benchmark's runs varied by more than {ROUGH:g}× from fastest to slowest on that machine; read it as rough."


def runtime_best(measured):
    """Of a runbench.py result's blaze builds, the one whose geometric mean is lowest: (its
    ratio of each benchmark, the geometric mean, how it was built), or None."""
    ours = [b for b in measured["builds"] if b not in ("release", "checked")]
    ratios = lambda b: {bench: row[b]["ratio"] for p in measured["programs"].values() for bench, row in p["benches"].items() if b in row}
    mean = lambda by: math.exp(sum(math.log(v) for v in by.values()) / len(by)) if by else None
    scored = [(mean(ratios(b)), b) for b in ours if ratios(b)]
    if not scored:
        return None
    geomean, build = min(scored)
    env = {"BLAZE_OPT": "3", **((measured.get("variants") or {}).get(build) or {})}
    named = ", ".join([f"-O{env.pop('BLAZE_OPT')}", *(f"{k}={v}" for k, v in env.items())])
    return ratios(build), geomean, named


def benchmarks(results):
    """Every benchmark any platform ran, in the order the platforms ran them."""
    seen = []
    for r in results.values():
        seen += [b for p in r["runbench"]["programs"].values() for b in p["benches"] if b not in seen]
    return seen


def runtime_chart(results):
    best = {p: b for p, r in results.items() if (b := runtime_best(r["runbench"]))}
    if not best:
        return None
    rows = [(bench, {p: b[0][bench] for p, b in best.items() if bench in b[0]}) for bench in benchmarks(results)]
    marked = {(bench, p) for p in best for pr in results[p]["runbench"]["programs"].values()
              for bench, row in pr["benches"].items() if rough(row)}
    builds = sorted({b[2] for b in best.values()})
    named = builds[0] if len(builds) == 1 else "; ".join(f"{p} {b[2]}" for p, b in best.items())
    return benchcharts.runtime(rows, f"blaze's LLVM at {named}. " + where({p: results[p] for p in best}),
                               marked, f"≈: that benchmark's runs varied by more than {ROUGH:g}×, so read it as rough.")


def runtime_tables(result):
    """One platform's runtime: each benchmark's times, ratios and memory, and each program's build."""
    measured = result["runbench"]
    checked = "checked" in measured["builds"]
    mem = lambda row, b: megabytes(row[b]["peak_mb"]) if row.get(b, {}).get("peak_mb") is not None else "-"
    lines = ["| Benchmark | `cargo build --release` | blaze, LLVM | blaze ÷ release |" + (" release with dev checks ÷ release |" if checked else "")
             + " Peak memory, release → blaze |",
             "|---|---:|---:|---:|" + ("---:|" if checked else "") + "---:|"]
    sized = []
    for name, program in measured["programs"].items():
        for bench, row in program["benches"].items():
            lines.append(f"| {bench} | {seconds(row['release']['median_s'])} | **{seconds(row['blaze']['median_s'])}** | "
                         f"{ratio_cell(row)} |" + ((f" {times(row['checked']['ratio'])} |" if "checked" in row else " - |") if checked else "") +
                         f" {mem(row, 'release')} → {mem(row, 'blaze')} |")
        info = program["builds"]
        if info.get("release", {}).get("size_mb") is not None and info.get("blaze", {}).get("size_mb") is not None:
            sized.append(f"| {name} | {seconds(info['release']['build_s'])} → {seconds(info['blaze']['build_s'])} | "
                         f"{info['release']['size_mb']} MB → {info['blaze']['size_mb']} MB | "
                         f"{megabytes(info['release'].get('build_peak_mb'))} → {megabytes(info['blaze'].get('build_peak_mb'))} | "
                         f"{megabytes(info['release'].get('build_largest_mb'))} → {megabytes(info['blaze'].get('build_largest_mb'))} |")
    geomean = measured.get("geomean") or {}
    if geomean.get("blaze"):
        lines.append(f"| **All benchmarks (geometric mean)** | | | **{times(geomean['blaze'])}** |" +
                     (f" {times(geomean['checked'])} |" if checked and geomean.get("checked") else (" |" if checked else "")) + " |")
    if sized:
        lines += ["", "What each program takes, release → blaze: its build from cold (for blaze, cargo running the build "
                  "scripts and proc macros, blaze's compile of every crate and the link), its size, and its build's memory "
                  "(the most the build's processes held at once, and the largest one process: rustc or blaze, or the linker).", "",
                  "| Program | Build | Size | Build, peak | Build, largest process |", "|---|---:|---:|---:|---:|", *sized]
    return lines


def rounds(result):
    rows = [row for p in result["runbench"]["programs"].values() for row in p["benches"].values()]
    return len(rows[0]["release"]["runs"]) if rows else None


def runtime_section(results):
    """docs/benchmarks.md's table of how fast what blaze's LLVM builds runs, against cargo's release
    build: blaze's time ÷ release's of each benchmark on each platform, then by platform the
    times, memory and builds."""
    lines = [RUN_START]
    if summary := runtime_chart(results):
        lines += [benchcharts.picture("runtime", f"How fast each benchmark runs built by blaze, against cargo build --release, by platform: {summary}"), ""]
    lines += ["How fast the programs run. Each program is built by `cargo build --release` and by blaze with LLVM at `-O3`, "
              "every crate of it compiled by blaze, with the release profile; each benchmark is then run several times, the "
              "builds taking turns, median. A run is published only when every program did what the release build's did. Each "
              "platform is a machine of its own, and each number is blaze's time ÷ release's on it: 1x is as fast.", "",
              "| Benchmark | " + " | ".join(NAMES) + " |", "|---|" + "---:|" * len(NAMES)]
    noisy = False
    for bench in benchmarks(results):
        cells = []
        for p in NAMES:
            row = next((pr["benches"][bench] for pr in (results[p]["runbench"]["programs"].values() if p in results else [])
                        if bench in pr["benches"]), None)
            cells.append(ratio_cell(row) if row and "blaze" in row else "-")
            noisy = noisy or bool(row and "blaze" in row and rough(row))
        lines.append(f"| {bench} | " + " | ".join(cells) + " |")
    means = [f"**{times(g)}**" if p in results and (g := (results[p]["runbench"].get("geomean") or {}).get("blaze")) else "-" for p in NAMES]
    lines += [f"| **All benchmarks (geometric mean)** | " + " | ".join(means) + " |", "", *([ROUGH_NOTE, ""] if noisy else []),
              *machines(results, lambda r: f", each benchmark run {rounds(r)} times" if rounds(r) else ""), "",
              "<details><summary>Times, memory and builds, by platform</summary>"]
    for p, r in results.items():
        lines += ["", f"**{p}**", "", *runtime_tables(r)]
    lines += ["", "Release with dev checks: `cargo build --release` with `-C overflow-checks=on -C debug-assertions=on`.", "",
              "</details>", "",
              "Measured by v1's runbench.py on each platform's benchmark machine, the last results before v2, "
              "which has no runtime bench yet. "
              f"Raw JSON: {raw_links(results, 'runtime')}.", RUN_END]
    return "\n".join(lines)


def charts():
    """Every chart again from the stored results."""
    results = stored_all()
    print("edits:", edits_chart(results))
    measured = {p: r for p, r in results.items() if has_memory(r)}
    if backend := memory_backend(measured):
        print("memory:", memory_chart(measured, backend))
        print("disk:", disk_chart(measured, backend))
    print("runtime:", runtime_chart(stored_all("runtime")))


def names():
    """The projects measured on their own (`--name`), from their stored results."""
    found = set()
    for path in os.listdir(BENCH_DIR):
        for p in NAMES:
            if path.endswith(f"-{slug(p)}.json") and not path.startswith("memory-"):
                stem = path[:-len(f"-{slug(p)}.json")].removeprefix("runtime").removeprefix("-")
                if stem:
                    found.add(stem)
    return sorted(found)


def write_sections(kind="", override=None, name=None):
    """Every table (and its chart) a stored result of `kind` feeds, from each platform's newest,
    with `override` (of project `name`, when it is measured on its own) in place of its stored one."""
    if kind == "runtime":
        if results := stored_all("runtime", override, name):
            splice(DOC, for_docs(runtime_section(results)), RUN_START, RUN_END)
    elif results := stored_all("", override, name):
        splice(README, headline(results), HEADLINE_START, HEADLINE_END)
        splice(README, pick_section(results), PICK_START, PICK_END)
        splice(DOC, for_docs(summary_section(results)), SUM_START, SUM_END)
        splice(DOC, doc_section(results))
        splice(DOC, for_docs(memory_section(results)), MEM_START, MEM_END)


def render():
    """Every table and chart again from the stored results."""
    write_sections()
    write_sections("runtime")


def main():
    argv = sys.argv[1:]
    stored_arg = options(argv, "--stored")
    name = options(argv, "--name")
    noise = float(options(argv, "--noise", "10"))
    max_age = float(options(argv, "--max-age", "24"))
    force, check = "--force" in argv, "--check" in argv
    if "--render" in argv:
        render()
        return
    if "--charts" in argv:
        charts()
        return
    paths = [a for a in argv if not a.startswith("--")]
    if len(paths) != 1:
        raise SystemExit(__doc__)
    result = json.load(open(paths[0]))
    runtime = "runbench" in result
    measured_on = platform(result)
    if measured_on not in NAMES:
        raise SystemExit(f"{paths[0]}: measured on {measured_on}, which is none of the platforms ({', '.join(NAMES)}; benchplatforms.py)")
    kind = "runtime" if runtime else ""
    stored_at = stored_arg or stored_path(measured_on, kind, name)
    if runtime:
        wrong = runtime_wrong(result) if result["runbench"] else ["runbench.py wrote nothing"]
    else:
        wrong = [f"{key}: {line}" for key, measured in result["results"].items() for line in measured.get("wrong", [])]
    tests = [line for line in wrong if not runtime and line.split(": ", 1)[1].startswith("tests (")]
    if programs := [line for line in wrong if line not in tests]:
        print("refused: a program said something else than blaze's cold build, or its run failed\n"
              + "\n".join(programs))
        sys.exit(2)
    if tests:
        print("written with tests that ended otherwise than under cargo (shown in the table):\n" + "\n".join(tests))
    stored = load_stored(stored_at)
    why = (["--force"] if force else []) + (runtime_differences if runtime else differences)(stored, result, noise, max_age)
    changed = bool(why)
    print(f"{measured_on}: " + ("\n".join(why[:40]) or f"every number within {noise:.0f}% (or 50 ms) of the stored run"))
    if changed and not check:
        os.makedirs(os.path.dirname(stored_at), exist_ok=True)
        if os.path.abspath(paths[0]) != os.path.abspath(stored_at):
            shutil.copyfile(paths[0], stored_at)
        write_sections(kind, result, name)
    if os.environ.get("GITHUB_OUTPUT"):
        open(os.environ["GITHUB_OUTPUT"], "a").write(f"changed={'true' if changed else 'false'}\n")
    print("changed" if changed else "unchanged")
    sys.exit(1 if check and changed else 0)


if __name__ == "__main__":
    main()
