#!/usr/bin/env python3
"""summ.py RESULT...: one line per result: no-op, body, signature, leaf, cold wall, cold steps, peak."""
import json, sys

for path in sys.argv[1:]:
    d = json.load(open(path))
    for name, r in d["results"].items():
        p = r["phases"]
        g = lambda k: p.get(k, {}).get("wall_s")
        cold = p.get("cold", {})
        mem = cold.get("memory", {}).get("blaze", {}).get("peak_mb")
        roots = p.get("body", {}).get("steps_ms", {}).get("roots")
        print(f"{d.get('commit','?')[:10]} {name}: noop={g('no-op')} body={g('body')} sig={g('signature')} "
              f"leaf={g('leaf')} generic={g('generic')} cold={g('cold')} std_made={cold.get('std_made_s')} "
              f"warm_store={cold.get('warm_store_s')} steps={cold.get('steps_s')} body_roots_ms={roots} peak_mb={mem} "
              f"failed={r.get('failed')}")
