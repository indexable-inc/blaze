#!/usr/bin/env python3
"""Collect every build script of the census projects' resolves.

Writes /root/census/scripts/<name>-<version>/{build.rs,meta.json} for each package with a
custom-build target, and /root/census/projects.json: project -> [package ids]."""
import json, os, subprocess, sys, shutil, concurrent.futures

W = "/root/census"
PROJECTS = {
    # smoke corpus (harness/smoke/smoke.json projects)
    "ripgrep": ("https://github.com/BurntSushi/ripgrep", "af60c2de9d85e7f3d81c78601669468cf02dabab"),
    "cargo": ("https://github.com/rust-lang/cargo", "ea2d97820c16195b0ca3fadb4319fe512c199a43"),
    "zed": ("https://github.com/zed-industries/zed", "aed37547d8543000494fb69101c05daec5891068"),
    "bun": ("https://github.com/oven-sh/bun", "c6da4a4d3010e5553438c60f6bd76d981976867c"),
    "servo": ("https://github.com/servo/servo", "aac43a3f31a259f04a574f5ec4e959c943ad7cc7"),
    # apps corpus (harness/apps/apps.json)
    "fd": ("https://github.com/sharkdp/fd", "b6ca4e02167833c2c492de6ae9d06b56488d4696"),
    "bat": ("https://github.com/sharkdp/bat", "62b439dae330a8c4d053ff7d8526a6e3697000b9"),
    "eza": ("https://github.com/eza-community/eza", "471bfbc7b03cbac8c738e8d9050edb06ee79132a"),
    "tokei": ("https://github.com/XAMPPRocky/tokei", "c14f744716272fadeb27a74443cdffa0af35f82f"),
    "delta": ("https://github.com/dandavison/delta", "3c2269c6b845913965575f11b0196d70ef5352f3"),
    "starship": ("https://github.com/starship/starship", "2d8741e4830273ff2bec1fe45e1e7fe6aa979cf4"),
    "nushell": ("https://github.com/nushell/nushell", "85aae22da8"),
    "helix": ("https://github.com/helix-editor/helix", "ba40e547426b0f9896c8bdc699a4ab11f2b37dbc"),
    "alacritty": ("https://github.com/alacritty/alacritty", "542147067dd016fc5f49120ef0ba605a98a4e3a7"),
    "ruff": ("https://github.com/astral-sh/ruff", "8d122c1fd2b8f16d494ff8a8f8ee6f264bba7ceb"),
    "rust-analyzer": ("https://github.com/rust-lang/rust-analyzer", "70e0c46be7"),
}


def sh(*cmd, cwd=None):
    return subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True).stdout


def checkout(name, repo, rev):
    d = f"{W}/src/{name}"
    if not os.path.isdir(d):
        os.makedirs(d)
        sh("git", "init", "-q", cwd=d)
        sh("git", "remote", "add", "origin", repo, cwd=d)
        try:
            sh("git", "fetch", "-q", "--depth", "1", "origin", rev, cwd=d)
        except subprocess.CalledProcessError:
            sh("git", "fetch", "-q", "origin", cwd=d)
        sh("git", "checkout", "-q", "FETCH_HEAD" if len(rev) == 40 else rev, cwd=d)
    return d


def metadata(d):
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=d,
                         capture_output=True, text=True)
    if out.returncode != 0:
        out = subprocess.run(["cargo", "metadata", "--format-version", "1"], cwd=d,
                             capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(out.stderr[-2000:])
    return json.loads(out.stdout)


def project(name):
    if name == "anim":
        d = f"{W}/src/anim"
    else:
        d = checkout(name, *PROJECTS[name])
    meta = metadata(d)
    resolved = {n["id"] for n in meta["resolve"]["nodes"]} if meta.get("resolve") else None
    found = []
    for pkg in meta["packages"]:
        if resolved is not None and pkg["id"] not in resolved:
            continue
        for t in pkg["targets"]:
            if "custom-build" not in t["kind"]:
                continue
            key = f"{pkg['name']}-{pkg['version']}"
            if pkg["source"] is None:
                key = f"{name}--{pkg['name']}-{pkg['version']}"
            out = f"{W}/scripts/{key}"
            os.makedirs(out, exist_ok=True)
            shutil.copy(t["src_path"], f"{out}/build.rs")
            build_deps = [dep["name"] for dep in pkg["dependencies"] if dep["kind"] == "build"]
            json.dump({"package": pkg["name"], "version": pkg["version"], "source": pkg["source"],
                       "edition": t.get("edition", pkg["edition"]), "build_deps": build_deps,
                       "src_path": t["src_path"], "links": pkg.get("links")},
                      open(f"{out}/meta.json", "w"))
            found.append(key)
    return name, found


def main():
    os.makedirs(f"{W}/scripts", exist_ok=True)
    names = list(PROJECTS) + ["anim"]
    result, failed = {}, {}
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        futs = {pool.submit(project, n): n for n in names}
        for f in concurrent.futures.as_completed(futs):
            n = futs[f]
            try:
                _, found = f.result()
                result[n] = found
                print(n, len(found), flush=True)
            except Exception as e:
                failed[n] = str(e)
                print(n, "FAILED", str(e)[-500:], flush=True)
    json.dump({"projects": result, "failed": failed}, open(f"{W}/projects.json", "w"), indent=1)
    print("DONE")


if __name__ == "__main__":
    main()
