#!/bin/bash
# fill.sh JOB...: repeats `fill a N JOB` until main's store for JOB exists (at most 6 runs), then the
# rest of the matrix given in /root/rb/specs.txt.
export PATH=/run/current-system/sw/bin:/usr/bin:$PATH HOME=/root
cd /root/rb
for job in "$@"; do
  for n in 1 2 3 4 5 6; do
    [ -d store-$job-main ] && break
    bash driver.sh "fill a $n $job"
  done
  [ -d store-$job-main ] || { echo "FILL-FAILED $job"; exit 1; }
done
echo "FILL-DONE $(date +%T)"
[ -f specs.txt ] && eval "bash driver.sh $(cat specs.txt)"
