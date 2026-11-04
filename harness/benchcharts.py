"""benchcharts.py: the benchmark charts of docs/benchmarks.md and the README, as SVG.

    benchcharts.py '{}'
    benchcharts.py '{"history": DIR, "charts": DIR, "readme": FILE, "doc": FILE}'

Two kinds of chart: blaze against cargo, from the results benchreport.py writes, and blaze
against its own past, the trends, from the history benchhistory.py appends to.

benchreport.py is the one caller of the first kind: every time it writes a table it writes the
chart of the same numbers next to it, so a chart is never older than its table. Each chart is
two files, `NAME-light.svg` and `NAME-dark.svg` under docs/benchmarks/charts, which the page
shows with `<picture>` by the reader's colour scheme. Hand-written SVG, standard library only:
nothing to pin, and a change of numbers is a readable diff. The two files of a chart differ only
in their `<style>`, and the same numbers always give the same bytes.

Every chart is the same chart (`compare`): blaze against cargo, as how many times better
blaze is, on a log axis around a line at 1×, the same as cargo. Right of the line blaze is
better, left of it worse, in every chart, and the axis says so under it.

  edits     how many times faster than incremental `cargo build` an edit builds, per project and
            platform: the median over the kinds of edit, the line from the slowest kind to the fastest
  memory    the most memory the work of an edit held at once, blaze's (the daemon, its workers,
            the command) against cargo's (cargo, rustc, the linker)
  disk      what each side keeps on disk once the edits are done: cargo's target directory against
            blaze's target directory, store and home
  runtime   how long the program blaze's LLVM built takes against `cargo build --release`'s

Each row has a bar per platform (benchplatforms.PLATFORMS, in that order), and a platform with
no numbers for the row says so in place of its bar. The first row is each platform's geometric
mean over the rows it measured. Each platform is its own machine, which the footer names: a bar
is comparable with the bars of its own colour.

The colours: a hue per platform, the same in every chart (blue, orange, teal: Okabe and Ito's
palette, apart under the common colour blindnesses), each tuned for the page it is drawn on. The
rest is GitHub's own greys, so a chart sits on a GitHub page like its tables do.

The trends (`trend`) are what the command line draws, with one typed JSON input (inline, a
file's path or `-`; every field optional, its default the repository's own path): from the
history files in `history` (docs/benchmarks/history), into `charts`, and the section between
`<!-- benchtrend:start -->` and `<!-- benchtrend:end -->` in `readme` and `doc`. Every workflow
that appends to the history runs it after the append and commits what it wrote
(.github/scripts/bench_commit.py). Per platform, from the runs of the machine of its last run
only (the last 120), as `benchhistory.py check` compares them:

  trend-<platform>-merge   the instructions of each project's edits and each stress crate's
                           check and build, from <platform>-merge.jsonl: per line the geometric
                           mean over its phases of each count against the line's first, as a
                           percent change on a linear axis, so projects of any size share it
  trend-<platform>         how long an edit takes, from <platform>.jsonl: per project (Cranelift)
                           the median over the kinds of edit, in milliseconds on a log axis
  trend-<name>-live        the median time from saving an edit to the program running it, per
                           live program, from <name>-live.jsonl, in the mode its last run measured

x is the runs, oldest first, one step each with dates under them; a run without a number is a
gap in its line, never a zero. At most eight lines, in eight colours: instructions fold the rest
into one line, their geometric mean; times name them under the chart. Each line ends in its name
and latest value, with the day when later runs lack it, and the alt text is each line's first
against its latest. A chart with fewer than two runs is not drawn: the section says so in its
place, and an old file of it is removed. Regressions are not marked: the history does not record
what `check` found.
"""
import json, math, os, sys
from dataclasses import dataclass, fields

from benchplatforms import NAMES, PLATFORMS

HERE = os.path.dirname(os.path.abspath(__file__))
CHARTS = os.path.join(os.path.dirname(HERE), "docs/benchmarks/charts")
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
# By scheme: a colour per platform (in NAMES' order), text, quieter text, grid lines, the 1× line,
# the band behind every other row, the page itself (canvas.default, the ring round a trend's
# points) and the eight colours of a trend's lines, in that order, which is the order that keeps
# neighbouring colours apart under the common colour blindnesses and in normal vision (checked
# against these pages with the dataviz skill's validator; the light yellow, aqua and pink are under
# 3:1 against white, so every line is labelled by name at its end as well). The greys are GitHub's
# (Primer's fg.default, fg.muted, border.muted, canvas.subtle) for its light and dark pages.
SCHEMES = {
    "light": dict(platforms=("#1f6fd0", "#d8700a", "#12a07a"), ink="#1f2328", dim="#59636e", grid="#e3e7eb",
                  rule="#59636e", band="#f6f8fa", page="#ffffff",
                  series=("#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948")),
    "dark": dict(platforms=("#5aa9f5", "#f0913a", "#34c69a"), ink="#f0f6fc", dim="#9198a1", grid="#262c36",
                 rule="#9198a1", band="#151b23", page="#0d1117",
                 series=("#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767")),
}
WIDTH, PAD, BAR, GAP = 760, 16, 12, 3
NOT_MEASURED = "not measured yet"
MACHINES = "Each colour is a machine of its own: compare bars of one colour, not across colours."


def esc(text):
    return str(text).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text_width(text, size):
    """About how wide `text` is in the sans-serif fonts above: enough to lay out a column."""
    narrow = sum(c in "ijlt.,:;|!' ()1" for c in text)
    return size * (0.58 * (len(text) - narrow) + 0.3 * narrow)


def wrap(text, size, width):
    lines, line = [], ""
    for word in text.split():
        if line and text_width(line + " " + word, size) > width:
            lines.append(line)
            line = word
        else:
            line = f"{line} {word}".strip()
    return lines + [line] if line else lines


def n(v):
    """A coordinate, to a tenth of a pixel."""
    return f"{v:.1f}".removesuffix(".0")


class Log:
    """A logarithmic axis from `lo` to `hi` onto pixels `x0` to `x1`."""

    def __init__(self, lo, hi, x0, x1):
        self.lo, self.hi, self.x0, self.x1 = math.log10(lo), math.log10(hi), x0, x1

    def __call__(self, v):
        return self.x0 + (math.log10(v) - self.lo) / (self.hi - self.lo) * (self.x1 - self.x0)


def factors(span):
    """The round factors past 1× an axis spanning `span` (its high ÷ its low) has room to mark,
    each side of the 1× line, up to 10⁶."""
    marks = (1, 2, 5) if span >= 30 else (1, 1.5, 2, 3, 5, 7) if span >= 6 else (1, 1.5, 2, 3, 4, 5, 7)
    return [round(10 ** d * m, 9) for d in range(7) for m in marks if 10 ** d * m > 1]


def geomean(values):
    return math.exp(sum(math.log(v) for v in values) / len(values))


def style(c):
    platforms = "".join(f".p{i}{{fill:{v}}}.w{i}{{stroke:{v}}}" for i, v in enumerate(c["platforms"]))
    return (f"<style>text{{font-family:{FONT};fill:{c['ink']}}}.d{{fill:{c['dim']}}}.b{{font-weight:600}}"
            f".v{{font-variant-numeric:tabular-nums}}.g{{stroke:{c['grid']}}}.r{{stroke:{c['rule']};stroke-width:1.25}}"
            f".z{{fill:{c['band']}}}.o{{fill:none;stroke-width:1.25}}{platforms}</style>")


def write_svg(name, title, height, shapes, styled, charts=CHARTS):
    """Write chart `name`'s two files into `charts`: `shapes` under the `<style>` that
    `styled(scheme's colours)` gives, one per scheme."""
    os.makedirs(charts, exist_ok=True)
    for scheme, c in SCHEMES.items():
        with open(os.path.join(charts, f"{name}-{scheme}.svg"), "w") as f:
            f.write(f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" viewBox="0 0 {WIDTH} {height}" '
                    f'role="img" aria-label="{esc(title)}">\n<title>{esc(title)}</title>\n{styled(c)}\n{shapes}\n</svg>\n')


def cell(better, low=None, high=None, detail="", rough=False):
    """One platform's bar of a row: how many times better blaze is (over 1, better; under 1,
    worse), the range of it from `low` to `high` when there is one, what to add after the factor,
    and whether the runs varied too much to read it closely (≈)."""
    return better, low, high, detail, rough


def compare(name, title, subtitle, rows, everything, words, same, hints, notes, digits=1):
    """Write chart `name`: `rows` is (label, {platform: cell(...)}), `everything` what the
    geometric-mean row stands for ("All projects"), `words` (better, worse) after a factor, `same`
    what the 1× line is, `hints` (left, right) what each side of it means, `notes` the footer.
    Returns every row in words, for the chart's alt text."""
    measured = [k for k in NAMES if any(k in by for _, by in rows)]
    means = {k: geomean([by[k][0] for _, by in rows if k in by]) for k in measured}

    def said(better, rough=False, detail=""):
        m = better if better >= 1 else 1 / better
        text = f"{m:.0f}×" if digits == 1 and m >= 9.95 else f"{m:.{digits}f}×"
        word = "" if float(text[:-1]) == 1 else f" {words[0] if better > 1 else words[1]}"
        return ("≈" if rough else "") + text + word + (f" · {detail}" if detail else "")

    lines = [((everything, "geometric mean"), True, [(NAMES.index(k), means[k], None, None, said(means[k])) for k in measured])]
    lines += [((label, None), False, [(NAMES.index(k), by[k][0], by[k][1], by[k][2], said(by[k][0], by[k][4], by[k][3]))
                                      if k in by else (NAMES.index(k), None, None, None, NOT_MEASURED) for k in measured])
              for label, by in rows]

    values = [x for _, _, bars in lines for _, v, low, high, _ in bars for x in (v, low, high) if x is not None]
    lo, hi = min(min(values) / 1.2, 0.5), max(max(values) * 1.2, 2)
    marks = factors(hi / lo)
    lo, hi = 1 / next(m for m in marks if m >= round(1 / lo, 9)), next(m for m in marks if m >= round(hi, 9))
    ticks = [1 / m for m in reversed(marks) if m <= round(1 / lo, 9)] + [1] + [m for m in marks if m <= hi]

    label_w = max(max(text_width(main, 13), text_width(sub or "", 11)) for (main, sub), _, _ in lines)
    value_w = max(text_width(text, 12) for _, _, bars in lines for *_, text in bars)
    x0, x1 = PAD + label_w + 16, WIDTH - PAD - value_w - 16
    scale = Log(lo, hi, x0, x1)
    one = scale(1)

    body, y = [f'<text x="{PAD}" y="26" font-size="17" class="b">{esc(title)}</text>'], 26
    for line in wrap(subtitle, 13, WIDTH - 2 * PAD):
        y += 19
        body.append(f'<text x="{PAD}" y="{y}" font-size="13" class="d">{esc(line)}</text>')
    at, y = PAD, y + 22
    for k in measured:
        width = 18 + text_width(k, 12.5)
        if at > PAD and at + width > WIDTH - PAD:
            at, y = PAD, y + 22
        body.append(f'<rect x="{at}" y="{y - 10}" width="12" height="12" rx="3" class="p{NAMES.index(k)}"/>')
        body.append(f'<text x="{n(at + 18)}" y="{y}" font-size="12.5">{esc(k)}</text>')
        at += width + 22

    top = y + 30
    heights = [len(bars) * BAR + (len(bars) - 1) * GAP + 16 for _, _, bars in lines]
    bottom = top + sum(heights)
    grid, marks_, labels = [], [], []
    for v in ticks:
        x = n(scale(v))
        grid.append(f'<line x1="{x}" y1="{top}" x2="{x}" y2="{bottom}" class="g"/>')
        labels.append(f'<text x="{x}" y="{bottom + 17}" font-size="11.5" text-anchor="middle" class="d v">'
                      f'{(v if v >= 1 else 1 / v):g}×</text>')
    row_y = top
    for i, (((main, sub), bold, bars), h) in enumerate(zip(lines, heights)):
        if i and i % 2 == 0:
            grid.insert(0, f'<rect x="{PAD}" y="{row_y}" width="{WIDTH - 2 * PAD}" height="{h}" class="z"/>')
        if i == 1:
            grid.append(f'<line x1="{PAD}" y1="{row_y}" x2="{WIDTH - PAD}" y2="{row_y}" class="r"/>')
        weight, strong = (" b", ' class="b"') if bold else ("", "")
        mid = row_y + h / 2
        labels.append(f'<text x="{n(x0 - 12)}" y="{n(mid + (-2 if sub else 4.5))}" font-size="13" text-anchor="end"{strong}>{esc(main)}</text>')
        if sub:
            labels.append(f'<text x="{n(x0 - 12)}" y="{n(mid + 12)}" font-size="11" text-anchor="end" class="d">{esc(sub)}</text>')
        by = row_y + 8
        for platform, value, low, high, text in bars:
            if value is None:
                marks_.append(f'<rect x="{n(one + 1)}" y="{n(by + 1.5)}" width="9" height="9" rx="2" class="o w{platform}"/>')
                labels.append(f'<text x="{n(one + 16)}" y="{n(by + BAR - 2)}" font-size="11.5" class="d">{esc(text)}</text>')
                by += BAR + GAP
                continue
            a, b = sorted((one, scale(min(max(value, lo), hi))))
            marks_.append(f'<rect x="{n(a)}" y="{n(by)}" width="{n(max(b - a, 2))}" height="{BAR}" rx="2" class="p{platform}"/>')
            # The label always reads rightwards: after a bar to the right, or just past the 1× line
            # for a bar to the left, where its row is empty.
            tx = b if value >= 1 else one
            if low is not None and high is not None:
                l, r = scale(max(low, lo)), scale(min(high, hi))
                marks_.append(f'<path d="M{n(l)} {n(by + BAR / 2)}H{n(r)}M{n(l)} {n(by + 2)}v8M{n(r)} {n(by + 2)}v8" class="o w{platform}"/>')
                tx = max(tx, r)
            labels.append(f'<text x="{n(tx + 6)}" y="{n(by + BAR - 2)}" font-size="12" class="v{weight}">{esc(text)}</text>')
            by += BAR + GAP
        row_y += h
    marks_.append(f'<line x1="{n(one)}" y1="{top - 8}" x2="{n(one)}" y2="{bottom}" class="r"/>')
    labels.append(f'<text x="{n(one)}" y="{top - 13}" font-size="11.5" text-anchor="middle" class="d">{esc(same)}</text>')
    left, right = f"← {hints[0]}", f"{hints[1]} →"
    # Under the row labels is empty, so the left hint may reach into it: with nothing worse than
    # 1× the line is near the plot's left edge, and the two hints must not overlap there.
    lx = max(one - 8, PAD + text_width(left, 12))
    rx = max(min(one + 8, WIDTH - PAD - text_width(right, 12)), lx + 16)
    labels.append(f'<text x="{n(lx)}" y="{bottom + 37}" font-size="12" text-anchor="end" class="d">{esc(left)}</text>')
    labels.append(f'<text x="{n(rx)}" y="{bottom + 37}" font-size="12" class="d">{esc(right)}</text>')
    y = bottom + 44
    for line in wrap(notes, 11.5, WIDTH - 2 * PAD):
        y += 17
        labels.append(f'<text x="{PAD}" y="{y}" font-size="11.5" class="d">{esc(line)}</text>')
    height = y + 14

    write_svg(name, title, height, "\n".join(body + grid + marks_ + labels), style)
    return "; ".join(f"{main}{' (' + sub + ')' if sub else ''}: " + ", ".join(f"{NAMES[k]} {text}" for k, v, *_, text in bars if v is not None)
                     for (main, sub), _, bars in lines)


def picture(name, alt, prefix="docs/benchmarks/charts"):
    """The markup for a chart: the dark file for a dark scheme, the light one otherwise."""
    return (f'<picture><source media="(prefers-color-scheme: dark)" srcset="{prefix}/{name}-dark.svg">'
            f'<img alt="{esc(alt)}" src="{prefix}/{name}-light.svg" width="{WIDTH}"></picture>')


def gigabytes(mb):
    return f"{mb / 1000:.1f} GB" if mb >= 1000 else f"{mb:.0f} MB"


def median(values):
    r = sorted(values)
    return r[len(r) // 2] if len(r) % 2 else math.sqrt(r[len(r) // 2 - 1] * r[len(r) // 2])


THE_LINE = "Right of the 1× line blaze is better, left of it worse; the scale is logarithmic."


def edits(rows, where):
    """`rows`: (project, {platform: [cargo seconds / blaze seconds of each kind of edit]}).
    `where` names each platform's machine. The chart's alt text is its own summary."""
    rows = [(p, {k: sorted(v) for k, v in by.items() if v}) for p, by in rows]
    rows = [(p, {k: cell(median(r), r[0], r[-1]) for k, r in by.items()}) for p, by in rows if by]
    if not rows:
        return None
    return compare("edits", "How much faster an edit builds than with cargo",
                   "A small edit to a real project, then an incremental build: cargo build's time ÷ blaze build's, "
                   "both warm. The bar is the median over the kinds of edit, the thin line runs from the slowest kind to "
                   f"the fastest. {THE_LINE}",
                   rows, "All projects", ("faster", "slower"), "as fast as cargo", ("blaze slower", "blaze faster"),
                   f"{MACHINES} {where}")


def against(rows):
    """(project, {platform: (cargo's MB, blaze's MB)}) as cells: how many times less blaze uses."""
    rows = [(p, {k: cell(a / b, detail=f"{gigabytes(b)} vs cargo's {gigabytes(a)}") for k, (a, b) in by.items() if a and b}) for p, by in rows]
    return [(p, by) for p, by in rows if by]


def memory(rows, where):
    """`rows`: (project, {platform: (cargo's peak MB, blaze's peak MB)}) during an edit."""
    rows = against(rows)
    if not rows:
        return None
    return compare("memory", "How much memory an edit's build takes, against cargo",
                   "The most memory every process doing the work held at once, blaze's against cargo's. blaze's is its "
                   "daemon, which keeps the project loaded between edits, its workers and the command; cargo's is cargo, "
                   f"rustc and the linker. {THE_LINE}",
                   rows, "All projects", ("less", "more"), "as much as cargo", ("blaze uses more", "blaze uses less"),
                   f"{MACHINES} {where}")


def disk(rows, where):
    """`rows`: (project, {platform: (cargo's MB on disk, blaze's MB on disk)}) once the edits are done."""
    rows = against(rows)
    if not rows:
        return None
    return compare("disk", "How much disk a project's build keeps, against cargo",
                   "What each side keeps on disk once the edits are done, blaze against cargo: cargo's target directory, "
                   f"and blaze's target directory, function store and home. {THE_LINE}",
                   rows, "All projects", ("less", "more"), "as much as cargo", ("blaze keeps more", "blaze keeps less"),
                   f"{MACHINES} {where}")


def runtime(rows, where, rough=(), rough_note=""):
    """`rows`: (benchmark, {platform: blaze's time / release's}); `rough`: the (benchmark, platform)
    whose runs varied too much to read closely, labelled ≈."""
    rows = [(b, {k: cell(1 / v, rough=(b, k) in rough) for k, v in by.items() if v}) for b, by in rows]
    rows = [(b, by) for b, by in rows if by]
    if not rows:
        return None
    marked = any(k in by for b, by in rows for k in by if (b, k) in rough)
    return compare("runtime", "How fast the programs blaze builds run, against cargo build --release",
                   "How long each benchmark takes built by blaze's LLVM against built by cargo build --release, on the "
                   f"same machine. {THE_LINE}",
                   rows, "All benchmarks", ("faster", "slower"), "as fast as release",
                   ("blaze's program slower", "blaze's program faster"),
                   f"{MACHINES} {where}" + (f" {rough_note}" if marked else ""), digits=2)


# The trends: blaze against its own past, from docs/benchmarks/history.

ROOT = os.path.dirname(HERE)
HISTORY = os.path.join(ROOT, "docs/benchmarks/history")
README = os.path.join(ROOT, "README.md")
DOC = os.path.join(ROOT, "docs/benchmarks.md")
TREND_START, TREND_END = "<!-- benchtrend:start -->", "<!-- benchtrend:end -->"
# At most this many lines in a chart (the colours there are), over the last this many runs.
MOST_SERIES, MOST_RUNS = 8, 120
PLOT_H = 230
# The phases of a project's result that are not edits, and those of a stress crate's.
NOT_EDITS = ("cold", "test", "test-edit")
STRESS_PHASES = ("check", "build")
# The live modes, the first a history has being the one charted: `cargo` is what
# live-latency.yaml times now, the others what it timed before.
LIVE_MODES = ("cargo", "socket", "burst", "replay", "cargo-run", "restart")
MONTHS = ("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec")


def read_history(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def one_machine(lines):
    """The last MOST_RUNS of `lines` from the machine (and cores) of the last one: a number is
    comparable with its own machine's only, as `benchhistory.py check` compares it."""
    if not lines:
        return []
    last = lines[-1]
    return [x for x in lines if x.get("machine") == last.get("machine") and x.get("cores") == last.get("cores")][-MOST_RUNS:]


def day(date):
    """`2026-10-22T03:36:58Z` as `Oct 22`."""
    return f"{MONTHS[int(date[5:7]) - 1]} {int(date[8:10])}"


def keys_in(lines, keep):
    """The result keys of `lines` that `keep` takes, in the order they first appear."""
    return [k for k in dict.fromkeys(k for x in lines for k in x["results"]) if keep(k)]


def counts_series(lines):
    """Per project, the instructions of its edits, and per stress crate those of its check and
    build: at each run the geometric mean, over the phases counted both then and at the line's
    first counted run, of the count over that first one; None where nothing was counted. Past
    MOST_SERIES lines, the rest are one line, their geometric mean."""
    keys = keys_in(lines, lambda k: not k.startswith("stress:")) + keys_in(lines, lambda k: k.startswith("stress:"))
    many = len({k.rsplit("/", 1)[-1] for k in keys}) > 1
    out = []
    for key in keys:
        stress = key.startswith("stress:")
        runs = []
        for x in lines:
            phases = (x["results"].get(key) or {}).get("phases") or {}
            runs.append({p: v["instructions"] for p, v in sorted(phases.items())
                         if (p in STRESS_PHASES if stress else p not in NOT_EDITS)
                         and isinstance(v.get("instructions"), (int, float)) and v["instructions"] > 0})
        first = next((r for r in runs if r), None)
        if first is None:
            continue
        values = [geomean(shared) if (shared := [r[p] / first[p] for p in r if p in first]) else None for r in runs]
        name = key.removeprefix("stress:") if many else key.removeprefix("stress:").rsplit("/", 1)[0]
        out.append((f"{name} {'check, build' if stress else 'edits'}", values))
    if len(out) > MOST_SERIES:
        rest = out[MOST_SERIES - 1:]
        folded = [geomean(xs) if (xs := [v[i] for _, v in rest if v[i] is not None]) else None for i in range(len(lines))]
        out = out[:MOST_SERIES - 1] + [(f"the other {len(rest)} (geometric mean)", folded)]
    return out, []


def edits_series(lines, backend="cranelift"):
    """Per project (of one backend), how long an edit took in milliseconds: the median over the
    kinds of edit it ran of each kind's median wall time. The projects past MOST_SERIES are
    named, not drawn."""
    out = []
    for key in keys_in(lines, lambda k: not k.startswith("stress:") and k.endswith("/" + backend)):
        values = []
        for x in lines:
            phases = (x["results"].get(key) or {}).get("phases") or {}
            walls = [v["wall_s"] * 1000 for p, v in sorted(phases.items()) if p not in NOT_EDITS and (v.get("wall_s") or 0) > 0]
            values.append(median(walls) if walls else None)
        if any(v is not None for v in values):
            out.append((key.rsplit("/", 1)[0], values))
    return out[:MOST_SERIES], [name for name, _ in out[MOST_SERIES:]]


def live_mode(lines):
    """The mode the last run measured, the first of LIVE_MODES when it measured more than one."""
    present = {m for program in lines[-1]["results"].values() for m, s in program.items() if s.get("p50_ms")} if lines else set()
    return next((m for m in LIVE_MODES + tuple(sorted(present)) if m in present), None)


def live_series(lines):
    """Per program, the median time from saving an edit to the program running it, in
    milliseconds, in the mode of live_mode."""
    mode = live_mode(lines)
    out = [(program, [((x["results"].get(program) or {}).get(mode) or {}).get("p50_ms") for x in lines])
           for program in sorted({p for x in lines for p, by in x["results"].items() if mode in by})]
    return out[:MOST_SERIES], [name for name, _ in out[MOST_SERIES:]]


def percent(ratio):
    p = (ratio - 1) * 100
    text = f"{p:+.0f}%" if abs(p) >= 9.95 else f"{p:+.1f}%"
    return "0%" if text in ("+0.0%", "-0.0%", "+0%", "-0%") else text.replace("-", "−")


def milliseconds(ms):
    if ms >= 1000:
        return f"{ms / 1000:.3g} s"
    return f"{ms:.1f} ms" if ms < 10 else f"{ms:.0f} ms"


def nice_ticks(lo, hi, most=6):
    """Round values from at or below `lo` to at or above `hi`, about `most` steps apart."""
    raw = (hi - lo) / most
    power = 10 ** math.floor(math.log10(raw))
    step = next(m * power for m in (1, 2, 2.5, 5, 10) if m * power >= raw)
    first, last = math.floor(lo / step + 1e-9), math.ceil(hi / step - 1e-9)
    return [round(i * step, 12) for i in range(first, last + 1)]


def log_ticks(lo, hi):
    """Round values between `lo` and `hi` for a logarithmic axis: from the coarsest set of marks
    per decade to the finest, the first that marks four to eight."""
    for marks in ((1,), (1, 3), (1, 2, 5), (1, 1.5, 2, 3, 5, 7), (1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 7, 8, 9)):
        ticks = [round(m * 10 ** d, 9) for d in range(math.floor(math.log10(lo)), math.ceil(math.log10(hi)) + 1)
                 for m in marks if lo <= m * 10 ** d <= hi]
        if 4 <= len(ticks) <= 8:
            return ticks
    return [round(v, 9) for v in nice_ticks(lo, hi, 4) if lo <= v <= hi]


def trend_style(c):
    lines = "".join(f".s{i}{{stroke:{v}}}.f{i}{{fill:{v}}}" for i, v in enumerate(c["series"]))
    return (f"<style>text{{font-family:{FONT};fill:{c['ink']}}}.d{{fill:{c['dim']}}}.b{{font-weight:600}}"
            f".v{{font-variant-numeric:tabular-nums}}.g{{stroke:{c['grid']}}}.r{{stroke:{c['rule']};stroke-width:1.25}}"
            f".l{{fill:none;stroke-width:2;stroke-linejoin:round;stroke-linecap:round}}"
            f".m{{stroke:{c['page']};stroke-width:1.5}}{lines}</style>")


def trend(name, title, subtitle, dates, series, axis, notes, charts=CHARTS):
    """Write the line chart `name` into `charts`. `series` is (label, [value or None per run]),
    a value per date of `dates` (the runs, oldest first, evenly spaced), None a gap in the line.
    `axis` is `change` (each value a ratio to the line's first, drawn as a percent on a linear
    axis) or `ms` (milliseconds on a logarithmic one). Returns the chart in words, for its alt
    text: each line's latest against its first."""
    said = percent if axis == "change" else milliseconds
    values = [v for _, vs in series for v in vs if v is not None]
    if axis == "change":
        ticks = nice_ticks(min(values + [0.995]), max(values + [1.005]), 5)
        lo, hi = ticks[0], ticks[-1]

        def y_of(v):
            return bottom - (v - lo) / (hi - lo) * PLOT_H
    else:
        lo, hi = min(values), max(values)
        if hi / lo < 1.5:
            lo, hi = lo / 1.2, hi * 1.2
        lo, hi = lo / 1.05, hi * 1.05
        ticks = log_ticks(lo, hi)
        scale = Log(lo, hi, 0, PLOT_H)

        def y_of(v):
            return bottom - scale(v)

    # The axis's marks with as many decimals as the finest of them needs, the same for all.
    tick_said = said
    if axis == "change":
        places = 0 if all(abs((t - 1) * 100 - round((t - 1) * 100)) < 1e-6 for t in ticks) else 1

        def tick_said(t):
            return "0%" if abs(t - 1) < 1e-9 else f"{(t - 1) * 100:+.{places}f}%".replace("-", "−")

    # Each line's first and last value, the last labelled with its day when a later run lacks it.
    ends = []
    for i, (label, vs) in enumerate(series):
        known = [(at, v) for at, v in enumerate(vs) if v is not None]
        if known:
            stale = f" ({day(dates[known[-1][0]])})" if known[-1][0] < len(vs) - 1 else ""
            ends.append((i, label, known[0][1], known[-1][1], stale))
    x0 = PAD + max(text_width(tick_said(t), 11.5) for t in ticks) + 10
    x1 = WIDTH - PAD - max(text_width(f"{label} {said(last)}{stale}", 12) for _, label, _, last, stale in ends) - 30

    body, y = [f'<text x="{PAD}" y="26" font-size="17" class="b">{esc(title)}</text>'], 26
    for line in wrap(subtitle, 13, WIDTH - 2 * PAD):
        y += 19
        body.append(f'<text x="{PAD}" y="{y}" font-size="13" class="d">{esc(line)}</text>')
    at, y = PAD, y + 24
    for i, (label, _) in enumerate(series):
        width = 22 + text_width(label, 12.5)
        if at > PAD and at + width > WIDTH - PAD:
            at, y = PAD, y + 22
        body.append(f'<path d="M{n(at)} {y - 4}h16" class="l s{i}"/>')
        body.append(f'<text x="{n(at + 22)}" y="{y}" font-size="12.5">{esc(label)}</text>')
        at += width + 20
    top = y + 34
    bottom = top + PLOT_H
    body.append(f'<text x="{PAD}" y="{top - 12}" font-size="11.5" class="d">'
                f'{"change since its first run" if axis == "change" else "milliseconds, logarithmic"}</text>')

    def x_of(at):
        return (x0 + x1) / 2 if len(dates) == 1 else x0 + 6 + at * (x1 - x0 - 12) / (len(dates) - 1)

    grid, marks_, labels = [], [], []
    for t in ticks:
        ty = y_of(t)
        grid.append(f'<line x1="{n(x0)}" y1="{n(ty)}" x2="{n(x1)}" y2="{n(ty)}" class="{"r" if axis == "change" and abs(t - 1) < 1e-9 else "g"}"/>')
        labels.append(f'<text x="{n(x0 - 8)}" y="{n(ty + 4)}" font-size="11.5" text-anchor="end" class="d v">{esc(tick_said(t))}</text>')
    if axis != "change":
        grid.append(f'<line x1="{n(x0)}" y1="{bottom}" x2="{n(x1)}" y2="{bottom}" class="g"/>')
    # Up to six dates under the axis, the first and the last among them, a day named once.
    count, shown = min(len(dates), 6), []
    for k in range(count):
        at = round(k * (len(dates) - 1) / max(count - 1, 1))
        if not shown or day(dates[at]) != day(dates[shown[-1]]):
            shown.append(at)
    for at in shown:
        tx = n(x_of(at))
        grid.append(f'<line x1="{tx}" y1="{bottom}" x2="{tx}" y2="{bottom + 4}" class="g"/>')
        labels.append(f'<text x="{tx}" y="{bottom + 18}" font-size="11.5" text-anchor="middle" class="d v">{esc(day(dates[at]))}</text>')
    labels.append(f'<text x="{n((x0 + x1) / 2)}" y="{bottom + 37}" font-size="12" text-anchor="middle" class="d">'
                  f'{len(dates)} runs, oldest to newest, one step each however far apart</text>')

    # A dot on every run while there are few, else only where a line ends or has no neighbour.
    every = len(dates) <= 40
    for i, (label, vs) in enumerate(series):
        path, pen = [], "M"
        for at, v in enumerate(vs):
            if v is None:
                pen = "M"
                continue
            path.append(f"{pen}{n(x_of(at))} {n(y_of(v))}")
            pen = "L"
        if len(path) > 1:
            marks_.append(f'<path d="{"".join(path)}" class="l s{i}"/>')
        for at, v in enumerate(vs):
            if v is None:
                continue
            alone = (at == 0 or vs[at - 1] is None) and (at == len(vs) - 1 or vs[at + 1] is None)
            last = all(w is None for w in vs[at + 1:])
            if every or alone or last:
                marks_.append(f'<circle cx="{n(x_of(at))}" cy="{n(y_of(v))}" r="{4 if last else 3}" class="m f{i}"/>')

    # A label right of each line's end, in the order of the ends, pushed apart to be read and
    # kept above the axis.
    ys = []
    for want, i in sorted((y_of(last), i) for i, _, _, last, _ in ends):
        ys.append([max(want, ys[-1][0] + 15) if ys else want, i])
    for k in range(len(ys) - 1, -1, -1):
        ys[k][0] = min(ys[k][0], (ys[k + 1][0] - 15) if k + 1 < len(ys) else bottom)
    by_index = {i: (label, last, stale) for i, label, _, last, stale in ends}
    for ly, i in ys:
        label, last, stale = by_index[i]
        marks_.append(f'<path d="M{n(x1 + 10)} {n(ly)}h10" class="l s{i}"/>')
        labels.append(f'<text x="{n(x1 + 25)}" y="{n(ly + 4)}" font-size="12">{esc(label)} '
                      f'<tspan class="d v">{esc(said(last) + stale)}</tspan></text>')

    y = bottom + 44
    for line in wrap(notes, 11.5, WIDTH - 2 * PAD):
        y += 17
        labels.append(f'<text x="{PAD}" y="{y}" font-size="11.5" class="d">{esc(line)}</text>')
    write_svg(name, title, y + 14, "\n".join(body + grid + marks_ + labels), trend_style, charts)
    span = f"{len(dates)} runs from {day(dates[0])} to {day(dates[-1])}"
    if axis == "change":
        return f"{span}, the latest against each line's first: " + "; ".join(f"{label} {said(last)}{stale}" for _, label, _, last, stale in ends)
    return f"{span}, the first to the latest: " + "; ".join(f"{label} {said(first)} to {said(last)}{stale}" for _, label, first, last, stale in ends)


def trend_jobs(history):
    """What there is to draw, in order: (platform name, history file's stem, kind)."""
    jobs = [job for p in PLATFORMS for job in ((p["name"], f"{p['slug']}-merge", "counts"), (p["name"], p["slug"], "edits"))]
    for file in sorted(os.listdir(history)) if os.path.isdir(history) else []:
        if file.endswith("-live.jsonl"):
            stem = file.removesuffix(".jsonl")
            where = stem.removesuffix("-live")
            system, _, arch = where.partition("-")
            jobs.append((next((p["name"] for p in PLATFORMS if p["slug"] == where), f"{system.capitalize()} {arch}"), stem, "live"))
    return jobs


def trend_charts(history=HISTORY, charts=CHARTS):
    """Draw every trend with two runs on one machine into `charts`, and remove the trend files
    there that were not drawn. Returns (name, alt text) of each chart drawn, in order, and in
    words each one not drawn and why."""
    drawn, waiting = [], []
    for platform_name, stem, kind in trend_jobs(history):
        lines = one_machine(read_history(os.path.join(history, f"{stem}.jsonl")))
        what = {"counts": "instructions per merge", "edits": "edit times", "live": "live-edit latency"}[kind]
        series, unshown = {"counts": counts_series, "edits": edits_series, "live": live_series}[kind](lines)
        runs = sum(any(vs[at] is not None for _, vs in series) for at in range(len(lines)))
        if runs < 2:
            waiting.append(f"{platform_name} {what} ({'one run' if runs else 'none yet'})")
            continue
        machine = lines[-1].get("machine") or "one machine"
        notes = (f"Only the runs on {machine} are drawn: a number compares with its own machine's, and a run on "
                 f"another machine starts a history of its own."
                 + (f" Not drawn, past {MOST_SERIES} lines: {', '.join(unshown)}." if unshown else ""))
        dates = [x["date"] for x in lines]
        name = f"trend-{stem}"
        if kind == "counts":
            title = f"Instructions per merge, {platform_name}"
            subtitle = ("What blaze's processes retired building each project's edits and each stress crate from nothing "
                        "(check, then build), in bench-counts.yaml's run on every merge to main: the geometric mean over "
                        "the phases of each count against the line's first. Up is more work; a gap is a run that counted "
                        "nothing.")
            alt = trend(name, title, subtitle, dates, series, "change", notes, charts)
        elif kind == "edits":
            title = f"How long an edit takes to build, {platform_name}"
            subtitle = ("blaze build after a small edit to a real project, warm, Cranelift, in bench-readme.yaml's runs: "
                        "per project the median over the kinds of edit of each kind's median time. Down is faster; a gap "
                        "is a run without that project.")
            alt = trend(name, title, subtitle, dates, series, "ms", notes, charts)
        else:
            title = f"From saving an edit to the program running it, {platform_name}"
            subtitle = (f"The median, per live program, over a night's edits in live_latency.py's {live_mode(lines)} mode, "
                        f"in live-latency.yaml's nightly runs. Down is faster; a gap is a night that program measured "
                        f"nothing.")
            alt = trend(name, title, subtitle, dates, series, "ms", notes, charts)
        drawn.append((name, f"{title}: {alt}"))
    keep = {f"{name}-{scheme}.svg" for name, _ in drawn for scheme in SCHEMES}
    for file in sorted(os.listdir(charts)) if os.path.isdir(charts) else []:
        if file.startswith("trend-") and file.endswith(".svg") and file not in keep:
            os.remove(os.path.join(charts, file))
    return drawn, waiting


def trend_section(drawn, waiting):
    """The README's trend section, its links from the repository's root."""
    lines = [TREND_START,
             "How blaze's own numbers move from run to run, each platform on its own benchmark machine: the instructions "
             "each merge to main retires, how long an edit takes and how soon a live edit runs, from "
             "[the history](docs/benchmarks/history) ([how it is kept](docs/benchmarks.md#history-and-regressions)). "
             "Every workflow that adds to the history draws these again and commits them with it.", ""]
    for name, alt in drawn:
        lines += [picture(name, alt), ""]
    if waiting:
        lines += [f"Not drawn until there are two runs on one machine: {', '.join(waiting)}.", ""]
    return "\n".join(lines[:-1] + [TREND_END])


@dataclass(frozen=True)
class Trends:
    """benchcharts.py's one input: what it draws from and writes to, every path from the
    current directory or absolute."""
    history: str = HISTORY
    charts: str = CHARTS
    readme: str = README
    doc: str = DOC


def typed(text):
    """A Trends from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith("{"):
        with open(text) as f:
            text = f.read()
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"benchcharts.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("benchcharts.py: the input is a JSON object")
    known = {f.name for f in fields(Trends)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"benchcharts.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    for key, v in sorted(value.items()):
        if not isinstance(v, str):
            sys.exit(f"benchcharts.py: field {key} is a string, not {json.dumps(v)}")
    return Trends(**value)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    given = typed(sys.argv[1])
    # benchreport.py imports this module; its splice is the one way a section is written.
    import benchreport
    drawn, waiting = trend_charts(given.history, given.charts)
    section = trend_section(drawn, waiting)
    benchreport.splice(given.readme, section, TREND_START, TREND_END)
    benchreport.splice(given.doc, benchreport.for_docs(section), TREND_START, TREND_END)
    print(f"drew {', '.join(name for name, _ in drawn) or 'nothing'}; not drawn: {', '.join(waiting) or 'nothing'}")


if __name__ == "__main__":
    main()
