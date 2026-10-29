# The bench as it ran in the ix VM (README.md): as a systemd unit, since it outlasts a shell.
# The spec's condition (80k functions, 300k calls, 1600x1000), then two that show what the
# frame time follows: a quarter of the pixels, and a quarter of the graph.
export PATH=/run/current-system/sw/bin:$PATH
set -euo pipefail
cd /root/fe
chromium=$(nix-shell -p chromium --run 'command -v chromium')
python3 crates/blaze-web/assets/dev/fixture.py '{"nodes": 20000, "edges": 75000, "seed": 1, "out": "fx/quarter.blzw"}'
run() {
  nix-shell -p "python3.withPackages(p:[p.playwright])" --run "python3 bench/results/2026-10-28-web-graph/run.py '$1'"
}
run "$(printf '{"assets": "crates/blaze-web/assets", "vendor": "vb/out", "fixture": "fx/anim-like.blzw", "out": "raw/80k-1600x1000", "chromium": "%s", "runs": 3, "settle_cap_s": 240}' "$chromium")"
run "$(printf '{"assets": "crates/blaze-web/assets", "vendor": "vb/out", "fixture": "fx/anim-like.blzw", "out": "raw/80k-800x500", "chromium": "%s", "runs": 3, "settle_cap_s": 120, "width": 800, "height": 500}' "$chromium")"
run "$(printf '{"assets": "crates/blaze-web/assets", "vendor": "vb/out", "fixture": "fx/quarter.blzw", "out": "raw/20k-1600x1000", "chromium": "%s", "runs": 3, "settle_cap_s": 120}' "$chromium")"
echo ALL DONE
