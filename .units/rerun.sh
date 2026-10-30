#!/bin/bash
# Reruns PR #1363's failed CI once main's run 40829153351 (which pushes the warm store) is over.
for i in $(seq 1 480); do
  s=$(gh run view 40829153351 --repo indexable-inc/blaze --json status -q .status 2>/dev/null)
  [ "$s" = "completed" ] && break
  sleep 60
done
gh run rerun 40793906396 --repo indexable-inc/blaze --failed
echo "rerun requested $(date -u)"
