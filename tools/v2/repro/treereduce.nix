# treereduce-rust, the reducer of tools/v2/repro/repro.py: the latest release, built from its
# source (upstream ships no aarch64-darwin binary and nixpkgs has no package). nixpkgs is the
# one flake.lock pins.
#
#   nix build --no-link --print-out-paths -f tools/v2/repro/treereduce.nix
let
  lock = builtins.fromJSON (builtins.readFile ../../../flake.lock);
  nixpkgs = lock.nodes.nixpkgs.locked;
  pkgs = import (builtins.fetchTarball {
    url = "https://github.com/${nixpkgs.owner}/${nixpkgs.repo}/archive/${nixpkgs.rev}.tar.gz";
    sha256 = nixpkgs.narHash;
  }) { };
  src = pkgs.fetchFromGitHub {
    owner = "langston-barrett";
    repo = "treereduce";
    rev = "47655579ae4a19e95d7c4f7911d7ded7719b6d22"; # v0.4.1
    hash = "sha256-htXcLyk0Om9JlQsMKEspF3d2EVRGET6aQLyTnZwWUgA=";
  };
in
pkgs.rustPlatform.buildRustPackage {
  pname = "treereduce-rust";
  version = "0.4.1";
  inherit src;
  cargoLock.lockFile = "${src}/Cargo.lock";
  cargoBuildFlags = [ "-p" "treereduce-rust" ];
  doCheck = false;
}
