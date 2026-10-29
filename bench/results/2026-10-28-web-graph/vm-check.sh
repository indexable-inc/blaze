# check.py as it was run in the ix VM, on a 5,000-function fixture.
export PATH=/run/current-system/sw/bin:$PATH
set -euo pipefail
cd /root/fe
chromium=$(nix-shell -p chromium --run 'command -v chromium')
python3 crates/blaze-web/assets/dev/fixture.py '{"nodes": 5000, "edges": 18000, "seed": 2, "out": "fx/small.blzw"}'
input=$(printf '{"assets": "%s/crates/blaze-web/assets", "fixtures": "%s/fx", "snapshot": "small.blzw", "records": "small.edit.cbor", "out": "%s/check", "chromium": "%s"}' "$PWD" "$PWD" "$PWD" "$chromium")
cd bench/results/2026-10-28-web-graph
nix-shell -p "python3.withPackages(p:[p.playwright])" --run "python3 check.py '$input'"
