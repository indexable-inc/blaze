set -e
# Both dependencies are registry crates so the dependency impl-discovery path is tested.
# The registry is local and needs no network access.
python3 setup.py
sed "s|@HERE@|${BM_HERE:-$PWD}|" .cargo/config.toml.in > .cargo/config.toml
