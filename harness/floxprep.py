#!/usr/bin/env python3
"""floxprep.py: the environment flox's Rust workspace builds in, from flox's own flake.

    floxprep.py CHECKOUT    prints the environment

flox (flox/flox) builds only inside Nix: its crates read some thirty variables with `env!` at
compile time (`NIX_BIN`, `FLOX_ACTIVATIONS_BIN`, `NIX_TARGET_SYSTEM`, the paths of its Nix
plugins, buildenv and package builder, ...), and its build scripts link OpenSSL and krb5 (GSSAPI,
through bindgen). Its developers build in `nix develop`, whose shell builds the `dev` output of
flox's own Nix, which no cache has, and runs a hook that writes into the checkout.

`prepare(checkout)` takes the environment of flox's own package build instead: the variables
`flox-cli.envs` of the checkout's flake, with the native inputs `rust-external-deps` names, in a
shell `nix print-dev-env` describes without running anything in the checkout. Every path in it
comes from cache.nixos.org or cache.flox.dev. Not taken: crane's prebuilt dependencies and its
cargo hooks, flox's own Rust toolchain (blaze builds with its own), and the `RUSTFLAGS` flox's
x86-64 Linux build gives rustc's linker. It returns the checkout and that environment.

The checkout is changed in one way: the build scripts of `factory-api-v1` and `catalog-api-v1`
write their OpenAPI clients into their own `src/client.rs`, which a sandboxed script may not, so
the workspace's manifest takes them out of the sandbox as docs/cargo.md says a user does
(`unsandboxed-build-scripts`).
The flake is read at the checkout's commit, so that change is not part of what Nix evaluates.
"""
import json, os, shlex, subprocess, sys

SUBSTITUTERS = ["--option", "extra-substituters", "https://cache.flox.dev?priority=50",
                "--option", "extra-trusted-public-keys", "flox-cache-public-1:7F4OyH7ZCnFhcze3fJdfyXYLQw/aV7GEed86nQ7IsOs="]

# The inputs are what pkgs/rust-external-deps/default.nix gives crane's build.
SHELL = """
let
  flox = builtins.getFlake "git+file://{checkout}?shallow=1&rev={rev}";
  pkgs = flox.packages.${{builtins.currentSystem}};
in
pkgs.floxDevelopmentPackages.callPackage
  ({{ mkShell, openssl, krb5, pkg-config, rustPlatform }}:
    mkShell ({{
      name = "flox-blaze";
      buildInputs = [ openssl.dev krb5.dev ];
      nativeBuildInputs = [ pkg-config rustPlatform.bindgenHook ];
    }} // removeAttrs pkgs.flox-cli.envs [ "RUSTFLAGS" ]))
  {{ }}
"""

# What `nix develop` leaves out of a shell's variables: the builder's own.
IGNORED = {"BASHOPTS", "HOME", "NIX_BUILD_TOP", "NIX_ENFORCE_PURITY", "NIX_LOG_FD", "NIX_REMOTE", "PPID",
           "SHELL", "SHELLOPTS", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "TEMP", "TEMPDIR", "TERM", "TMP",
           "TMPDIR", "TZ", "UID"}

# What the workspace's manifest is given: the scripts that write into their own source.
OUTSIDE = '\n[workspace.metadata.blaze.sandbox]\nunsandboxed-build-scripts = ["factory-api-v1", "catalog-api-v1"]\n'


def prepare(checkout):
    """The checkout, with OUTSIDE in its manifest, and the environment its builds need (see the
    module's text)."""
    checkout = os.path.abspath(checkout)
    rev = subprocess.run(["git", "-C", checkout, "rev-parse", "HEAD"], capture_output=True, text=True,
                         check=True).stdout.strip()
    argv = ["nix", "print-dev-env", "--json", "--impure", *SUBSTITUTERS, "--expr",
            SHELL.format(checkout=checkout, rev=rev)]
    done = subprocess.run(argv, capture_output=True, text=True)
    if done.returncode:
        raise SystemExit(f"floxprep: `nix print-dev-env` of flox's build environment exited {done.returncode}:\n"
                         + done.stderr[-3000:])
    variables = json.loads(done.stdout)["variables"]
    env = {name: v["value"] for name, v in variables.items() if v["type"] == "exported" and name not in IGNORED}
    env["PATH"] = f"{env['PATH']}:{os.environ['PATH']}"
    manifest = os.path.join(checkout, "Cargo.toml")
    with open(manifest) as f:
        text = f.read()
    if OUTSIDE not in text:
        if "[workspace.metadata.blaze.sandbox]" in text:
            raise SystemExit(f"floxprep: {manifest} has a [workspace.metadata.blaze.sandbox] that is not "
                             f"floxprep's: clone it again")
        with open(manifest, "a") as f:
            f.write(OUTSIDE)
    return checkout, env


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    _, env = prepare(sys.argv[1])
    for name, value in sorted(env.items()):
        print(f"export {name}={shlex.quote(value)}")
