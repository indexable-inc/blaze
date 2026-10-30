"""Re-applies one commit's change to a nix units file on top of another version of it.

Input (JSON on stdin): {"before": path, "after": path, "onto": path, "out": path,
"rename": [old_version, new_version]}. The change is after minus before, with the version
renamed in both; dict keys added or changed and list items added are applied to onto.
"""
import json, sys

args = json.load(sys.stdin)
old, new = args["rename"]


def load(path):
    return json.loads(open(path).read().replace(old, new))


before, after, onto = load(args["before"]), load(args["after"]), json.load(open(args["onto"]))


def key(item):
    if isinstance(item, dict) and set(item) == {"name", "unit"}:
        return (item["unit"], item["name"], "")
    if isinstance(item, dict):
        return (item.get("name", ""), item.get("version", ""), json.dumps(item, sort_keys=True))
    return ("", "", json.dumps(item, sort_keys=True))


def same(x):
    return json.dumps(x, sort_keys=True)


def apply(b, a, t, path="$"):
    if isinstance(a, dict) and isinstance(b, dict) and isinstance(t, dict):
        for k, v in a.items():
            if k not in b:
                if k in t and t[k] != v:
                    sys.exit(f"{path}.{k}: added here and different on main")
                t[k] = v
            elif v != b[k]:
                if k not in t:
                    sys.exit(f"{path}.{k}: changed here, gone on main")
                t[k] = apply(b[k], v, t[k], f"{path}.{k}")
        for k in b:
            if k not in a:
                sys.exit(f"{path}.{k}: removed here; not handled")
        return t
    if isinstance(a, list) and isinstance(b, list) and isinstance(t, list):
        seen = {same(it) for it in b}
        added = [it for it in a if same(it) not in seen]
        out = list(t)
        if len(a) - len(added) != len(b):
            # An item changed in place: lock packages are matched by name and version.
            by_name = {it.get("name"): it for it in b if isinstance(it, dict)}
            for it in added:
                if isinstance(it, dict) and it.get("name") in by_name:
                    at = next((i for i, x in enumerate(out) if isinstance(x, dict)
                               and x.get("name") == it["name"] and x.get("version") == it.get("version")), None)
                    if at is None:
                        sys.exit(f"{path}: {it['name']} changed here, gone on main")
                    out[at] = apply(by_name[it["name"]], it, out[at], f"{path}[{it['name']}]")
                else:
                    out.append(it)
            return out
        have = {same(it) for it in out}
        out += [it for it in added if same(it) not in have]
        if all(isinstance(x, str) for x in out) or all(isinstance(x, dict) and set(x) == {"name", "unit"} for x in out):
            out.sort(key=key)
        return out
    return a


merged = apply(before, after, onto)
for system, data in merged["systems"].items():
    units = data["units"]
    for name, unit in units.items():
        for dep in unit.get("deps", []):
            if dep["unit"] not in units:
                sys.exit(f"{system}: {name} depends on {dep['unit']}, which is not a unit")
open(args["out"], "w").write(json.dumps(merged, indent=1, sort_keys=True) + "\n")
print("merged")
