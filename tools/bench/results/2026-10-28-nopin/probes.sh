export PATH=/run/current-system/sw/bin:$PATH
export HOME=/root/hn BLAZE_HOME=/root/np/bh-norust-after
P=/root/np/proj-probes
rm -rf $P; mkdir -p $P/src
cat > $P/Cargo.toml <<'EOF'
[package]
name = "probes"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
thiserror = "2"
rustversion = "1"
num-traits = "0.2"
indexmap = "2"
version_check = "0.9"
proc-macro-error2 = "2"
EOF
cat > $P/src/main.rs <<'EOF'
#[derive(Debug, thiserror::Error)]
#[error("bad {0}")]
struct Bad(u32);

#[rustversion::since(1.60)]
fn since() -> &'static str { "since 1.60" }
#[rustversion::before(1.60)]
fn since() -> &'static str { "before 1.60" }
#[rustversion::nightly]
fn chan() -> &'static str { "nightly" }
#[rustversion::not(nightly)]
fn chan() -> &'static str { "not nightly" }

fn main() -> anyhow::Result<()> {
    let m: indexmap::IndexMap<u32, u32> = [(1, 2)].into_iter().collect();
    println!("{} {} {:?} {}", since(), chan(), m, num_traits::Float::sqrt(4.0f64));
    println!("{:?}", version_check::Version::read().map(|v| v.to_string()));
    let e = anyhow::Error::new(Bad(3)).context("ctx");
    println!("{e:#}");
    Ok(())
}
EOF
cd $P
unshare -m bash -c '
for f in $(which -a rustc cargo rustdoc rustup 2>/dev/null | sort -u); do mount --bind /dev/null "$f"; done
B=/root/np/nopin-norustc/target/release/blaze
strace -f -e trace=execve -o /root/np/probes.strace $B build > /root/np/probes.build 2>&1 &
S=$!; sleep 2
while pgrep -x blaze >/dev/null; do sleep 1; done
sleep 1; kill -9 $S; tail -5 /root/np/probes.build
$B run -q; echo "run rc=$?"
'
echo "execs by program:"; grep -o 'execve("[^"]*"' /root/np/probes.strace | sed 's/.*\///; s/"$//' | sort | uniq -c | sort -rn | head -20
echo PROBED
