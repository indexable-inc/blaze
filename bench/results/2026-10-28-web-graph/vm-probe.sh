# What a cosmos.gl live update costs once the GPU is drained (README.md, "The live-update stall"),
# at 80k and 20k functions: one cosmos run each with short phases, then bench.probeLive.
export PATH=/run/current-system/sw/bin:$PATH
set -euo pipefail
cd /root/fe
chromium=$(nix-shell -p chromium --run 'command -v chromium')
python3 crates/blaze-web/assets/dev/fixture.py '{"nodes": 20000, "edges": 75000, "seed": 1, "out": "fx/quarter.blzw"}'
for f in anim-like quarter; do
  input=$(printf '{"assets": "crates/blaze-web/assets", "vendor": "crates/blaze-web/assets/vendor", "fixture": "fx/%s.blzw", "out": "raw-probe/%s", "chromium": "%s", "runs": 1, "arms": ["cosmos"], "settle_cap_s": 20, "panzoom_s": 2, "live_s": 10, "probe": 12}' "$f" "$f" "$chromium")
  nix-shell -p "python3.withPackages(p:[p.playwright])" --run "python3 bench/results/2026-10-28-web-graph/run.py '$input'"
done
echo ALL DONE
