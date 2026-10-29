# bisect.sh NAME BAD GOOD METRIC THRESHOLD: git bisect run over first-parent BAD..GOOD.
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
name=$1 bad=$2 good=$3 metric=$4 thr=$5
cd /root/perf/blaze
git bisect reset >/dev/null 2>&1
git bisect start --first-parent "$bad" "$good"
git bisect run bash /root/perf/pred.sh "$metric" "$thr"
git bisect log > /root/perf/bisect-$name.log
git bisect reset
echo "BISECT $name DONE"
