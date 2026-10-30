#!/usr/bin/env python3
"""servoprep.py: servo's checkout as a user builds it with blaze.

    servoprep.py CHECKOUT    prepares CHECKOUT in place

The checkout is changed in one way: stylo's build script (properties/build.py) writes its
CSS property documentation into the target directory's `doc/`, outside its OUT_DIR, which a
sandboxed script may not, so the workspace's manifest takes it out of the sandbox as
docs/cargo.md says a user does (`unsandboxed-build-scripts`). Its builds need no environment
of their own.
"""
import os, sys

# What the workspace's manifest is given: the script that writes outside its OUT_DIR.
OUTSIDE = '\n[workspace.metadata.blaze.sandbox]\nunsandboxed-build-scripts = ["stylo"]\n'


def prepare(checkout):
    """The checkout, with OUTSIDE in its manifest, and no environment."""
    checkout = os.path.abspath(checkout)
    manifest = os.path.join(checkout, "Cargo.toml")
    with open(manifest) as f:
        text = f.read()
    if OUTSIDE not in text:
        if "[workspace.metadata.blaze.sandbox]" in text:
            raise SystemExit(f"servoprep: {manifest} has a [workspace.metadata.blaze.sandbox] that is not "
                             f"servoprep's: clone it again")
        with open(manifest, "a") as f:
            f.write(OUTSIDE)
    return checkout, {}


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    prepare(sys.argv[1])
