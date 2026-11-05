export PATH=/run/current-system/sw/bin:$PATH HOME=/root
set -u
H=/root/hermetic
(cd $H/br && nix develop -c cargo fmt -p hir-ty-codegen -p blaze-engine -p blaze-cargo 2>&1 | tail -3)
(cd $H/br && nix develop -c cargo build --release -p blaze-engine -p blaze-cache 2>&1 | grep -E '^error' -A12 | head -40; nix develop -c cargo build --release -p blaze-engine -p blaze-cache 2>&1 | tail -1)
ls -la $H/br/target/release/blaze || { echo "BUILD br FAILED"; exit 1; }
for p in ripgrep fd just; do
  rm -rf $H/ws-br/$p; mkdir -p $H/ws-br; cp -r $H/cargo-$p $H/ws-br/$p; rm -rf $H/ws-br/$p/target
done
DEV=$(nix develop $H/cur -c bash -c 'echo $PATH')
ms() { local s=$(date +%s%N); "$@" > /tmp/final-last.out 2>&1; echo "$(( ($(date +%s%N) - s) / 1000000 ))"; }
one() { # label(cur|all|pre|cargo) project
  local label=$1 p=$2
  case $label in
    cargo) cd $H/cargo-$p && ms /run/current-system/sw/bin/cargo test --workspace;;
    *) local tree=cur tests=""; [ $label = pre ] && tree=pre; [ $label = br ] && tree=br; [ $label = all ] && tests=all
       local ws=$tree
       cd $H/ws-$ws/$p && PATH=$H/$tree/target/release:$DEV BLAZE_TESTS=$tests BLAZE_HOME=$H/home-$ws-$p BLAZE_CACHE=$H/store-$ws-$p ms blaze test --workspace;;
  esac; }
med() { printf "%s\n" "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)] " (min " a[1] ", max " a[NR] ")"}'; }
edit() { python3 - "$@" <<'PY'
import re, sys
root, file, anchor, n = sys.argv[1:]
path = f"{root}/{file}"; text = open(path).read()
text = re.sub(r"\n *std::hint::black_box\(\d+u32\);", "", text)
assert anchor in text, anchor
if n != "0":
    text = text.replace(anchor, anchor + f"\n        std::hint::black_box({n}u32);", 1)
open(path, "w").write(text)
PY
}
for spec in "ripgrep|crates/matcher/src/lib.rs|pub fn len(&self) -> usize {" \
            "fd|src/dir_entry.rs|pub fn path(&self) -> &Path {" \
            "just|src/lexer.rs|  fn lexeme(&self) -> &'src str {"; do
  IFS='|' read -r p file anchor <<< "$spec"
  for d in $H/ws-br/$p $H/ws-cur/$p $H/ws-pre/$p $H/cargo-$p; do edit $d "$file" "$anchor" 0; done
  for l in br br cur cur pre pre cargo cargo; do one $l $p >/dev/null; done
  b=(); c=(); r=(); g=()
  for round in 1 2 3 4 5 6 7 8; do
    if [ $((round % 2)) = 1 ]; then order="br cur pre cargo"; else order="cargo pre cur br"; fi
    for l in $order; do t=$(one $l $p); case $l in br) b+=($t);; cur) c+=($t);; pre) r+=($t);; cargo) g+=($t);; esac; done
  done
  echo "$p warm: branch $(med ${b[@]})  main $(med ${c[@]})  pre836 $(med ${r[@]})  cargo $(med ${g[@]})"
  echo "$p warm raw branch=${b[*]} main=${c[*]} pre=${r[*]} cargo=${g[*]}"
  b=(); c=(); r=(); g=()
  for round in 1 2 3 4 5 6 7; do
    n=$((8000 + round))
    for d in $H/ws-br/$p $H/ws-cur/$p $H/ws-pre/$p $H/cargo-$p; do edit $d "$file" "$anchor" $n; done
    if [ $((round % 2)) = 1 ]; then order="br cur pre cargo"; else order="cargo pre cur br"; fi
    for l in $order; do t=$(one $l $p); case $l in br) b+=($t);; cur) c+=($t);; pre) r+=($t);; cargo) g+=($t);; esac; done
  done
  echo "$p edit: branch $(med ${b[@]})  main $(med ${c[@]})  pre836 $(med ${r[@]})  cargo $(med ${g[@]})"
  echo "$p edit raw branch=${b[*]} main=${c[*]} pre=${r[*]} cargo=${g[*]}"
  for d in $H/ws-br/$p $H/ws-cur/$p $H/ws-pre/$p $H/cargo-$p; do edit $d "$file" "$anchor" 0; done
  (cd $H/ws-br/$p && PATH=$H/br/target/release:$DEV BLAZE_HOME=$H/home-br-$p BLAZE_CACHE=$H/store-br-$p blaze test --workspace >/dev/null 2>&1; cp target/blaze/tests.summary.json $H/final-$p.summary.json; PATH=$H/br/target/release:$DEV BLAZE_HOME=$H/home-br-$p BLAZE_CACHE=$H/store-br-$p blaze test --workspace --why > $H/final-$p.why.txt 2>&1)
done
# fd: where a warm run's time goes
cd $H/ws-br/fd
for i in 1 2; do PATH=$H/br/target/release:$DEV BLAZE_HOME=$H/home-br-fd BLAZE_CACHE=$H/store-br-fd BLAZE_STATS=1 blaze test --workspace -v > $H/fd-warm.out 2>&1; done
grep -n -E 'Running|Doc-tests|finished in|test result|Cached|run every time' $H/fd-warm.out | head -40
echo A901-DONE
