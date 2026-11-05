#!/usr/bin/env bash
# q.sh SCRIPT ARGS...: runs one job at a time (builds and measurements never overlap).
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
exec flock /root/perf/lock bash "$@"
