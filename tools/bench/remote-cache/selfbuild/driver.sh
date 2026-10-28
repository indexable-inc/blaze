#!/bin/bash
# driver.sh "ARM SHAPE ROUND JOB" ...: runs bench.py for each, in order, inside nix develop, each
# in a systemd unit of its own so that its cgroup's memory.peak is that run's, on 4 CPUs (a
# GitHub runner's count).
export PATH=/run/current-system/sw/bin:/usr/bin:$PATH HOME=/root
cd /root/rb
for spec in "$@"; do
  echo "== $(date +%T) $spec"
  sync; echo 3 > /proc/sys/vm/drop_caches
  systemd-run --wait --collect --unit "rb-run-${spec// /-}" -E PATH="$PATH" -E HOME=/root -p AllowedCPUs=0-3 \
    -p MemoryMax=13G -p MemorySwapMax=0 \
    -p StandardOutput=append:/root/rb/runs.log -p StandardError=append:/root/rb/runs.log \
    /run/current-system/sw/bin/nix develop /root/rb/src --command python3 /root/rb/bench.py $spec 2>&1
done
echo "DRIVER-DONE $(date +%T)"
