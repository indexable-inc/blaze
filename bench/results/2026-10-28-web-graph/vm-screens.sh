# The screenshots as they were taken in the ix VM (README.md, "Screenshots"), as a systemd unit.
export PATH=/run/current-system/sw/bin:$PATH
set -euo pipefail
cd /root/fe
chromium=$(nix-shell -p chromium --run 'command -v chromium')
python3 crates/blaze-web/assets/dev/fixture.py '{"nodes": 80000, "edges": 300000, "seed": 1, "out": "fx/anim-like.blzw"}'
input=$(printf '{"assets": "crates/blaze-web/assets", "fixtures": "fx", "snapshot": "anim-like.blzw", "records": "anim-like.edit.cbor", "out": "screens", "chromium": "%s", "layout_cap_s": 240, "slow": 20, "mid_ms": 27000, "settle_s": 300}' "$chromium")
nix-shell -p "python3.withPackages(p:[p.playwright])" --run "python3 bench/results/2026-10-28-web-graph/screens.py '$input'"
