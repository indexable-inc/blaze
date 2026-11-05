# job.sh UNIT SCRIPT ARGS...: SCRIPT under q.sh as a transient unit capped below the VM's memory,
# so a runaway build or bench is killed instead of hanging the guest. Log: /root/perf/UNIT.log.
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
unit=$1; shift
systemd-run --collect --unit "$unit" -p MemoryMax=14G -p MemorySwapMax=0 \
  -p StandardOutput=file:/root/perf/$unit.log -p StandardError=file:/root/perf/$unit.log \
  bash /root/perf/q.sh "$@"
