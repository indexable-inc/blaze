#!/usr/bin/env bash
# measure-frames.sh OUT BIN...: builds the breakers with each blaze binary and reads its frames.
set -uo pipefail
export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH HOME=/root
out=/root/ax/out/$1; shift
mkdir -p $out
cd /root/ax/trees/langtests
for bin in "$@"; do
  export TMPDIR=/root/ax/tmp/$bin; rm -rf $TMPDIR; mkdir -p $TMPDIR
  BLAZE_BIN=/root/ax/bin/$bin/blaze-engine timeout 3600 nix --extra-experimental-features 'nix-command flakes' develop -c \
    python3 harness/breakers.py branch_frames frame_of_array < /dev/null > $out/$bin.breakers.log 2>&1
  work=$(find $TMPDIR -maxdepth 2 -type d -name 'blaze-diff-*' | head -1)
  ls $work/*.o > $out/$bin.objects
  FRAMES_OF=6nested,4pair,frame_of_array python3 /root/ax/frames.py $work/*.o > $out/$bin.frames.json
  echo "$bin: $(tail -1 $out/$bin.breakers.log) work=$work"
done
