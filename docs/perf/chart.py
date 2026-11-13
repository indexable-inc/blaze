"""Draws docs/perf/dedupe-bench.{svg,png} from docs/perf/dedupe-compare.txt, the raw output
of blaze-bench's paired `compare` runs (see PERF.md). Run: python3 docs/perf/chart.py"""
import re
import statistics
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

here = Path(__file__).parent
text = (here / "dedupe-compare.txt").read_text()
row = re.compile(r"^(dedupe/\S+)\s+\[\s*([\d.]+) ms \.\.\.\s*([\d.]+) ms \]")
runs = {"upstream": {}, "previous": {}, "this": {}}
section = None
for line in text.splitlines():
    if line.startswith("## vs upstream"):
        section = "upstream"
    elif line.startswith("## vs previous"):
        section = "previous"
    m = row.match(line)
    if not m or section is None:
        continue
    name, base, cand = m.group(1), float(m.group(2)), float(m.group(3))
    runs[section].setdefault(name, []).append(base)
    if section == "upstream":
        runs["this"].setdefault(name, []).append(cand)

scenarios = ["dedupe/generic_chains", "dedupe/generic_chains_keeping", "dedupe/long_chains"]
labels = ["generic_chains\n(9 rounds)", "generic_chains_keeping\n(9 rounds, originals)", "long_chains\n(65 rounds)"]
series = [("upstream main @615873fa", "upstream", "#9aa0a6"),
          ("perf/speedup v1 (worklist)", "previous", "#f4a259"),
          ("perf/speedup v2 (this)", "this", "#d9480f")]
fig, ax = plt.subplots(figsize=(9, 4.8), dpi=150)
width = 0.26
for i, (label, key, color) in enumerate(series):
    means = [statistics.mean(runs[key][s]) for s in scenarios]
    lo = [m - min(runs[key][s]) for m, s in zip(means, scenarios)]
    hi = [max(runs[key][s]) - m for m, s in zip(means, scenarios)]
    xs = [x + (i - 1) * width for x in range(len(scenarios))]
    bars = ax.bar(xs, means, width, yerr=[lo, hi], capsize=3, label=label, color=color)
    for x, m, s in zip(xs, means, scenarios):
        up = statistics.mean(runs["upstream"][s])
        note = f"{m:.0f} ms" if key == "upstream" else f"{m:.0f} ms\n{up / m:.1f}x"
        ax.text(x, max(runs[key][s]) + 6, note, ha="center", va="bottom", fontsize=7)
ax.set_xticks(range(len(scenarios)), labels, fontsize=8)
ax.set_ylabel("mean ms per iteration (lower is better)")
ax.set_title("blaze code dedupe (blaze-bench, paired runs, 3 seeds; bars: min-max of seed means)", fontsize=9)
ax.set_ylim(0, max(max(v) for v in runs["upstream"].values() if v) * 1.25)
ax.legend(fontsize=8, frameon=False)
ax.spines[["top", "right"]].set_visible(False)
fig.text(0.01, 0.01, "8-vCPU Intel Xeon VM, Linux 6.12, nightly-2026-10-27; speedups vs upstream", fontsize=6.5, color="#555")
fig.tight_layout(rect=(0, 0.03, 1, 1))
for ext in ("svg", "png"):
    fig.savefig(here / f"dedupe-bench.{ext}")
for key in runs:
    for s in scenarios:
        print(key, s, [round(v, 1) for v in runs[key][s]], round(statistics.mean(runs[key][s]), 1))
