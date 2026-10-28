#!/bin/bash
# xfer.sh STORE PROJECT ROUNDS: pushes STORE to the gateway as PROJECT, then pulls it ROUNDS times
# into an empty store, each timed. Rows to /root/rb/xfer.tsv: time op project secs rc bytes.
export PATH=/run/current-system/sw/bin:/root/rb/bin:$PATH HOME=/root
STORE=$1 PROJECT=$2 ROUNDS=${3:-2}
SPEC=blaze+https://blaze-gateway.indexable.workers.dev/t/rcache-test/v3
export BLAZE_REMOTE_REQUIRED=1 BLAZE_REMOTE_TOKEN=$(cat /root/.rcache-token) BLAZE_REMOTE_TRUST=$(cat /root/.rcache-pub)
export BLAZE_REMOTE_SIGN=/root/.rcache-sign
row() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$@" >> /root/rb/xfer.tsv; }
t=$(date +%s.%N); BLAZE_CACHE=$STORE blaze-engine cache push $SPEC $PROJECT; rc=$?
row push $PROJECT "$(awk "BEGIN { print $(date +%s.%N) - $t }")" $rc "$(du -sb $STORE | cut -f1)"
for r in $(seq 1 $ROUNDS); do
  dst=/root/rb/pulled-$PROJECT; rm -rf $dst; mkdir -p $dst
  sync; echo 3 > /proc/sys/vm/drop_caches
  t=$(date +%s.%N); BLAZE_CACHE=$dst blaze-engine cache pull $SPEC $PROJECT; rc=$?
  row pull $PROJECT "$(awk "BEGIN { print $(date +%s.%N) - $t }")" $rc "$(du -sb $dst | cut -f1)"
done
echo XFER-DONE
