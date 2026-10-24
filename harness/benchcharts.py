"""benchcharts.py: the README's benchmark charts, as SVG, from the results benchreport.py writes.

benchreport.py is the one caller: every time it writes a table into the README it writes the
chart of the same numbers next to it, so a chart is never older than its table. Each chart is
two files, `NAME-light.svg` and `NAME-dark.svg` under docs/benchmarks/charts, which the README
shows with `<picture>` by the reader's colour scheme. Hand-written SVG, standard library only:
nothing to pin, and a change of numbers is a readable diff. The two files of a chart differ only
in their `<style>`, and the same numbers always give the same bytes.

Every chart is the same chart (`compare`): ra-jit against cargo, as how many times better
ra-jit is, on a log axis around a line at 1×, the same as cargo. Right of the line ra-jit is
better, left of it worse, in every chart, and the axis says so under it.

  edits     how many times faster than incremental `cargo build` an edit builds, per project and
            platform: the median over the kinds of edit, the line from the slowest kind to the fastest
  memory    the most memory the work of an edit held at once, ra-jit's (the daemon, its workers,
            the command) against cargo's (cargo, rustc, the linker)
  disk      what each side keeps on disk once the edits are done: cargo's target directory against
            ra-jit's target directory, store and home
  runtime   how long the program ra-jit's LLVM built takes against `cargo build --release`'s

Each row has a bar per platform (benchplatforms.PLATFORMS, in that order), and a platform with
no numbers for the row says so in place of its bar. The first row is each platform's geometric
mean over the rows it measured. Each platform is its own machine, which the footer names: a bar
is comparable with the bars of its own colour.

The colours: a hue per platform, the same in every chart (blue, orange, teal: Okabe and Ito's
palette, apart under the common colour blindnesses), each tuned for the page it is drawn on. The
rest is GitHub's own greys, so a chart sits in the README like its tables do.
"""
import math, os

from benchplatforms import NAMES

HERE = os.path.dirname(os.path.abspath(__file__))
CHARTS = os.path.join(os.path.dirname(HERE), "docs/benchmarks/charts")
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
# By scheme: a colour per platform (in NAMES' order), text, quieter text, grid lines, the 1× line,
# and the band behind every other row. The greys are GitHub's (Primer's fg.default, fg.muted,
# border.muted, canvas.subtle) for its light and dark pages.
SCHEMES = {
    "light": dict(platforms=("#1f6fd0", "#d8700a", "#12a07a"), ink="#1f2328", dim="#59636e", grid="#e3e7eb",
                  rule="#59636e", band="#f6f8fa"),
    "dark": dict(platforms=("#5aa9f5", "#f0913a", "#34c69a"), ink="#f0f6fc", dim="#9198a1", grid="#262c36",
                 rule="#9198a1", band="#151b23"),
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


def cell(better, low=None, high=None, detail="", rough=False):
    """One platform's bar of a row: how many times better ra-jit is (over 1, better; under 1,
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

    shapes = "\n".join(body + grid + marks_ + labels)
    os.makedirs(CHARTS, exist_ok=True)
    for scheme, c in SCHEMES.items():
        with open(os.path.join(CHARTS, f"{name}-{scheme}.svg"), "w") as f:
            f.write(f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" viewBox="0 0 {WIDTH} {height}" '
                    f'role="img" aria-label="{esc(title)}">\n<title>{esc(title)}</title>\n{style(c)}\n{shapes}\n</svg>\n')
    return "; ".join(f"{main}{' (' + sub + ')' if sub else ''}: " + ", ".join(f"{NAMES[k]} {text}" for k, v, *_, text in bars if v is not None)
                     for (main, sub), _, bars in lines)


def picture(name, alt, prefix="docs/benchmarks/charts"):
    """The README's markup for a chart: the dark file for a dark scheme, the light one otherwise."""
    return (f'<picture><source media="(prefers-color-scheme: dark)" srcset="{prefix}/{name}-dark.svg">'
            f'<img alt="{esc(alt)}" src="{prefix}/{name}-light.svg" width="{WIDTH}"></picture>')


def gigabytes(mb):
    return f"{mb / 1000:.1f} GB" if mb >= 1000 else f"{mb:.0f} MB"


def median(values):
    r = sorted(values)
    return r[len(r) // 2] if len(r) % 2 else math.sqrt(r[len(r) // 2 - 1] * r[len(r) // 2])


THE_LINE = "Right of the 1× line ra-jit is better, left of it worse; the scale is logarithmic."


def edits(rows, where):
    """`rows`: (project, {platform: [cargo seconds / ra-jit seconds of each kind of edit]}).
    `where` names each platform's machine. The chart's alt text is its own summary."""
    rows = [(p, {k: sorted(v) for k, v in by.items() if v}) for p, by in rows]
    rows = [(p, {k: cell(median(r), r[0], r[-1]) for k, r in by.items()}) for p, by in rows if by]
    if not rows:
        return None
    return compare("edits", "How much faster an edit builds than with cargo",
                   "A small edit to a real project, then an incremental build: cargo build's time ÷ cargo ra-jit build's, "
                   "both warm. The bar is the median over the kinds of edit, the thin line runs from the slowest kind to "
                   f"the fastest. {THE_LINE}",
                   rows, "All projects", ("faster", "slower"), "as fast as cargo", ("ra-jit slower", "ra-jit faster"),
                   f"{MACHINES} {where}")


def against(rows):
    """(project, {platform: (cargo's MB, ra-jit's MB)}) as cells: how many times less ra-jit uses."""
    rows = [(p, {k: cell(a / b, detail=f"{gigabytes(b)} vs cargo's {gigabytes(a)}") for k, (a, b) in by.items() if a and b}) for p, by in rows]
    return [(p, by) for p, by in rows if by]


def memory(rows, where):
    """`rows`: (project, {platform: (cargo's peak MB, ra-jit's peak MB)}) during an edit."""
    rows = against(rows)
    if not rows:
        return None
    return compare("memory", "How much memory an edit's build takes, against cargo",
                   "The most memory every process doing the work held at once, ra-jit's against cargo's. ra-jit's is its "
                   "daemon, which keeps the project loaded between edits, its workers and the command; cargo's is cargo, "
                   f"rustc and the linker. {THE_LINE}",
                   rows, "All projects", ("less", "more"), "as much as cargo", ("ra-jit uses more", "ra-jit uses less"),
                   f"{MACHINES} {where}")


def disk(rows, where):
    """`rows`: (project, {platform: (cargo's MB on disk, ra-jit's MB on disk)}) once the edits are done."""
    rows = against(rows)
    if not rows:
        return None
    return compare("disk", "How much disk a project's build keeps, against cargo",
                   "What each side keeps on disk once the edits are done, ra-jit against cargo: cargo's target directory, "
                   f"and ra-jit's target directory, function store and home. {THE_LINE}",
                   rows, "All projects", ("less", "more"), "as much as cargo", ("ra-jit keeps more", "ra-jit keeps less"),
                   f"{MACHINES} {where}")


def runtime(rows, where, rough=(), rough_note=""):
    """`rows`: (benchmark, {platform: ra-jit's time / release's}); `rough`: the (benchmark, platform)
    whose runs varied too much to read closely, labelled ≈."""
    rows = [(b, {k: cell(1 / v, rough=(b, k) in rough) for k, v in by.items() if v}) for b, by in rows]
    rows = [(b, by) for b, by in rows if by]
    if not rows:
        return None
    marked = any(k in by for b, by in rows for k in by if (b, k) in rough)
    return compare("runtime", "How fast the programs ra-jit builds run, against cargo build --release",
                   "How long each benchmark takes built by ra-jit's LLVM against built by cargo build --release, on the "
                   f"same machine. {THE_LINE}",
                   rows, "All benchmarks", ("faster", "slower"), "as fast as release",
                   ("ra-jit's program slower", "ra-jit's program faster"),
                   f"{MACHINES} {where}" + (f" {rough_note}" if marked else ""), digits=2)
