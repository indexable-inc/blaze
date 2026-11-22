# The tree's gate, run by hand (`just lint`): the ratchet tools of scripts/src/bin, each taking one
# JSON value (rule 42). CI runs the same list (tools/gate.txt) through the same runner.
# The toolchain is pins/toolchain's, the one home of the pin; every command has a timeout (rule 69).

set shell := ["bash", "-euo", "pipefail", "-c"]

# Build the scripts with the pinned nightly; prints their directory.
[private]
scripts:
    #!/usr/bin/env bash
    set -euo pipefail
    toolchain=$(< pins/toolchain)
    timeout 1200 cargo +"$toolchain" build -q -p scripts --bins >&2 || { echo "lint: building the scripts failed or passed 1200 s (cargo +$toolchain build -p scripts --bins)" >&2; exit 1; }
    echo target/debug

# The whole gate: every row of tools/gate.txt, the one list blaze.yaml's `scripts` job runs too,
# one line each with its time; exits nonzero naming each that failed. The runner first checks that
# this recipe and the job run nothing beside it (scripts/src/bin/gate.rs). Each row has its own
# timeout; this one bounds the whole.
lint:
    #!/usr/bin/env bash
    set -euo pipefail
    bin=$(just scripts)
    timeout 7200 "$bin/gate" "{\"command\": \"run\", \"scripts\": \"$bin\"}"

# Rewrites every ratchet file from the tree, in order: a row never rises by a write.
lint-write:
    #!/usr/bin/env bash
    set -euo pipefail
    bin=$(just scripts)
    timeout 300 "$bin/thirdparty" '{"command": "write"}'
    meta=$(mktemp)
    timeout 240 cargo metadata --format-version 1 --offline --locked > "$meta"
    timeout 120 "$bin/thirdparty" "{\"command\": \"reach-write\", \"metadata\": \"$meta\"}"
    rm -f "$meta"
    for command in write tests-write heap-write reach-write producers-write sansio-write; do
        timeout 300 "$bin/ambient" "{\"command\": \"$command\"}"
    done
    for tool in stops clones casts recursion untyped; do
        timeout 300 "$bin/$tool" '{"command": "write"}'
    done

# The sans-IO table, a row a crate, then the totals and the three no_std numbers.
lint-report:
    #!/usr/bin/env bash
    set -euo pipefail
    bin=$(just scripts)
    timeout 300 "$bin/ambient" '{"command": "sansio-report"}'
    # Per member: dependency entries not on `workspace = true`, `[package]`/`[lints]` keys not inherited.
    timeout 120 "$bin/thirdparty" '{"command": "manifests-report"}'
    # D180's table: each third-party name, who names it, its closure and exclusive closure.
    meta=$(mktemp)
    timeout 240 cargo metadata --format-version 1 --offline --locked > "$meta"
    timeout 120 "$bin/thirdparty" "{\"command\": \"reach-report\", \"metadata\": \"$meta\"}"
    rm -f "$meta"
