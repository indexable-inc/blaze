import hashlib
import json
import pathlib
import tarfile

for name in ("maker", "dep"):
    archive = pathlib.Path("registry") / f"{name}-0.1.0.crate"
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "w:gz") as output:
        output.add(pathlib.Path("published") / name, arcname=f"{name}-0.1.0")
    deps = []
    if name == "dep":
        deps.append({"name": "maker", "req": "=0.1.0", "features": [],
                     "optional": False, "default_features": True, "target": None,
                     "kind": "normal", "registry": None})
    path = "3/d/dep" if name == "dep" else "ma/ke/maker"
    index = pathlib.Path("registry/index") / path
    index.parent.mkdir(parents=True, exist_ok=True)
    index.write_text(json.dumps({"name": name, "vers": "0.1.0", "deps": deps,
                                "cksum": hashlib.sha256(archive.read_bytes()).hexdigest(),
                                "features": {}, "yanked": False}) + "\n")
