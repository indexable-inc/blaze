{
  description = "blaze: cargo build, test and run from a daemon that keeps your workspace compiled, with live edits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # The one toolchain blaze is pinned to: the dated nightly in harness/toolchain
        # (`nightly-YYYY-MM-DD`, moved by harness/toolchain_bump.py, which also moves the
        # rust-overlay input to one that has it). rust-src is the sysroot source rust-analyzer
        # reads `core` from.
        pin = pkgs.lib.removePrefix "nightly-" (pkgs.lib.fileContents ./harness/toolchain);
        rust = pkgs.rust-bin.nightly.${pin}.default.override {
          extensions = [ "rust-src" ];
        };
        # The same nightly with rustc_codegen_cranelift, for the setups bench.py compares with
        # (`--setups`, docs/benchmarks.md).
        nightly = pkgs.rust-bin.nightly.${pin}.default.override {
          extensions = [ "rust-src" "rustc-codegen-cranelift-preview" ];
        };
        # The one LLVM the `llvm` feature builds against, for every build of it: the dev shells
        # here, CI (`.#llvm`, .github/actions/toolchain), the flake's `dist` and the release.
        # crates/hir-ty-codegen/llvm.json pins it: LLVM 23.1.4, built for each target by
        # .github/scripts/llvm.py (the llvm workflow) and fetched here by its sha256. Its static libraries, headers, llvm-config,
        # llvm-dwarfdump and llvm-pdbutil; LLVM_SYS_231_PREFIX names it, and
        # crates/hir-ty-codegen/build.rs links it statically and refuses any other. Its tools were
        # built on Ubuntu 22.04 with the C++ runtime linked in: on Linux they are pointed at the
        # store's libc.
        llvmPin = builtins.fromJSON (builtins.readFile ./crates/hir-ty-codegen/llvm.json);
        llvm =
          let pin = llvmPin.targets.${triple} or (throw "crates/hir-ty-codegen/llvm.json has no LLVM for ${triple}: run the llvm workflow");
          in pkgs.stdenv.mkDerivation {
            pname = "blaze-llvm";
            inherit (llvmPin) version;
            src = pkgs.fetchurl { url = "${llvmPin.release}/${pin.name}"; inherit (pin) sha256; };
            nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.autoPatchelfHook ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.stdenv.cc.cc.lib ];
            dontConfigure = true;
            dontBuild = true;
            # The archives are what the release links: not stripped or rewritten.
            dontStrip = true;
            installPhase = "cp -r . $out";
          };
        llvmEnv = { LLVM_SYS_231_PREFIX = "${llvm}"; };
        # What harness/bench.py runs in on the benchmark machine (docs/benchmarks.md): the
        # pinned toolchain, LLVM 23 for `--features llvm`, and the C side the projects' build
        # scripts use. Nothing here wraps rustc (no sccache) or replaces the default linker:
        # mold and wild are on PATH for the setups that ask for them with `-fuse-ld`. Every
        # library directory here costs every link a `realpath` in Nix's linker wrapper, which
        # cargo pays and blaze does not: what only one project needs goes in a shell of its own.
        bench = llvmEnv // {
          packages = [
            rust
            pkgs.python3
            pkgs.git
            pkgs.pkg-config
            pkgs.openssl
            llvm
            pkgs.libffi
            pkgs.libxml2
            pkgs.zlib
            pkgs.ncurses
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            # bevy's audio, input and windowing crates link these.
            pkgs.alsa-lib
            pkgs.udev
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.mold
            pkgs.wild
          ] ++ [
            # dx, for harness/hotpatch.py's subsecond (Dioxus hot-patching).
            pkgs.dioxus-cli
          ];
          BLAZE_NIGHTLY = "${nightly}";
          # A C compiler as a distribution ships it: the wrapper's _FORTIFY_SOURCE warns at the
          # -O0 of a debug build, which fails configure probes (jemalloc's, in fd).
          hardeningDisable = [ "all" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };
        src = self;
        # The one version, from the workspace's Cargo.toml.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
        # The one build of blaze, the release workflow's recipe (.github/scripts/build_release.py):
        # the `dist` profile optimized with the profile tools/pgo/profiles.json pins for this system,
        # fetched by its sha256. The binaries run each other from next to themselves, unwrapped.
        # `--nix`: they load nixpkgs' glibc, so the release's floor of RHEL 9's glibc is not theirs
        # (build_release.py checks that instead; release.yaml keeps the floor).
        triple = {
          x86_64-linux = "x86_64-unknown-linux-gnu";
          aarch64-linux = "aarch64-unknown-linux-gnu";
          aarch64-darwin = "aarch64-apple-darwin";
        }.${system};
        pgo = builtins.fromJSON (builtins.readFile ./tools/pgo/profiles.json);
        pgoProfile =
          let pin = pgo.targets.${triple} or (throw "tools/pgo/profiles.json has no profile for ${triple}: run the pgo workflow");
          in pkgs.fetchurl { url = "${pgo.release}/${pin.name}"; inherit (pin) sha256; };
        dist = rustPlatform.buildRustPackage (llvmEnv // {
          pname = "blaze-dist";
          inherit version src;
          # `blaze-engine --version` names the commit (a dirty tree has none of its own).
          BLAZE_GIT_SHA = self.shortRev or self.dirtyShortRev or "";
          cargoLock.lockFile = ./Cargo.lock;
          # otool: build_release.py checks that no binary loads libLLVM.
          nativeBuildInputs = [ pkgs.python3 ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.cctools ];
          doCheck = false;
          # cargo-auditable wraps rustc for the workspace's crates, which changes their symbols:
          # the profile would find none of them.
          auditable = false;
          buildPhase = ''
            runHook preBuild
            python3 -I .github/scripts/build_release.py --target ${triple} --profdata ${pgoProfile} --out shipped --nix
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p $out/bin $out/share/blaze
            cp shipped/* $out/bin/
            # The standard library's source, which blaze finds above its programs
            # (crates/blaze-cargo/src/cargo/toolchain/library.rs).
            cp -r library $out/share/blaze/library
            runHook postInstall
          '';
        });
        # What `nix profile install` puts on PATH: the same binaries, with the toolchain.
        daemon = pkgs.runCommand "blaze-${version}" {
          nativeBuildInputs = [ pkgs.makeWrapper ];
          meta.mainProgram = "blaze";
        } ''
          mkdir -p $out/bin
          for bin in ${dist}/bin/*; do
            makeWrapper $bin $out/bin/$(basename $bin) --prefix PATH : ${rust}/bin
          done
        '';
        # `blaze` with the toolchain and the other programs first on PATH.
        run = pkgs.writeShellScriptBin "blaze" ''
          export PATH=${daemon}/bin:${rust}/bin:$PATH
          exec ${daemon}/bin/blaze "$@"
        '';

        # eframe/winit dlopen these at run time.
        guiLibs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (with pkgs; [
          libGL
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
        ]);
        libPath = pkgs.lib.makeLibraryPath guiLibs;

        # One derivation per cargo unit (tools/nix/units.nix, tools/nix/units/*.json from tools/nix/unit_graph.py):
        # editing blaze's own source rebuilds only blaze, never the crates it depends on. The
        # demo's window is built this way; `daemon-units` is blaze built this way, `release`
        # without PGO, for working on the flake: what ships is `dist`.
        units = json: import ./tools/nix/units.nix {
          inherit pkgs rust guiLibs json;
          repo = ./.;
          gitSha = self.shortRev or self.dirtyShortRev or "";
        };
        daemonUnits = units ./tools/nix/units/daemon.json;
        lifeUnits = units ./tools/nix/units/life.json;
        # blaze's binaries run each other from next to themselves, found by
        # their own path: the binaries are copied together, not wrapped or linked. The toolchain
        # has to be on PATH (the launcher puts it there).
        daemon-units = pkgs.runCommand "blaze-units" { } ''
          mkdir -p $out/bin
          cp -L ${daemonUnits.bins}/bin/* $out/bin/
        '';
        life-bin = pkgs.runCommand "blaze-life-bin" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
          mkdir -p $out/bin
          makeWrapper ${builtins.head lifeUnits.roots}/bin/life $out/bin/life \
            --prefix LD_LIBRARY_PATH : ${libPath}
        '';

        # The demo's own source (and the crate it links), copied to a writable directory at run
        # time; its crates.io dependencies are vendored from Cargo.lock so that needs no network.
        lifeSource = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [ ./demo/life ./crates/blaze-live ];
        };
        lifeVendor = pkgs.rustPlatform.importCargoLock { lockFile = ./demo/life/Cargo.lock; };
        # `nix run github:indexable-inc/blaze#life`: the Game of Life window. On macOS on AArch64 the
        # daemon builds the copy and edits to it show up live; elsewhere the prebuilt window runs.
        life = pkgs.writeShellScriptBin "blaze-life" ''
          export BLAZE_LIFE_SOURCE=${lifeSource} BLAZE_LIFE_VENDOR=${lifeVendor}
          export BLAZE_LIFE_DAEMON_BINS=${dist}/bin BLAZE_LIFE_PREBUILT=${life-bin}/bin/life
          export BLAZE_LIFE_PATH=${rust}/bin${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ":${pkgs.stdenv.cc}/bin"}
          export BLAZE_LIFE_LIBS=${libPath}
          exec ${pkgs.python3}/bin/python3 -I ${./tools/nix/life_launch.py} "$@"
        '';
      in {
        packages = {
          # `nix profile install github:indexable-inc/blaze`: blaze and the programs it runs, so
          # `blaze build` works. `#blaze` names it too.
          default = daemon;
          blaze = daemon;
          inherit dist daemon run life daemon-units life-bin;
          # The pinned LLVM: its tools (llvm-dwarfdump), which CI puts on PATH where the harnesses
          # read debug info, and the prefix a build of the `llvm` feature links.
          inherit llvm;
        };
        # `nix run github:indexable-inc/blaze -- build` is `blaze build`.
        apps.default = {
          type = "app";
          program = "${run}/bin/blaze";
        };
        apps.life = {
          type = "app";
          program = "${life}/bin/blaze-life";
        };
        apps.blaze = {
          type = "app";
          program = "${run}/bin/blaze";
        };
        # `nix develop`: the pinned LLVM (LLVM_SYS_231_PREFIX, its tools on PATH) and a C
        # toolchain with libstdc++.a, so `cargo build --features llvm` links it in. Rust is
        # rustup at the pinned nightly: plain `cargo` is harness/toolchain's and `cargo +<toolchain>` works.
        devShells.default = pkgs.mkShell (llvmEnv // {
          packages = [ pkgs.rustup pkgs.python3 llvm ];
          RUSTUP_TOOLCHAIN = pkgs.lib.fileContents ./harness/toolchain;
          # As in the bench shell: the wrapper's _FORTIFY_SOURCE fails configure probes at -O0 in
          # the C code of the projects the harnesses build.
          hardeningDisable = [ "all" ];
        } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          # rustup's rustc links x86-64 Linux with its own rust-lld, past the wrapper of the
          # shell's linker that writes the store's libraries into the binary's RUNPATH: without
          # this the binary finds neither the store's glibc nor its libgcc_s when it runs.
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-Clinker-features=-lld";
        });
        # What builds, runs and inspects wasm32 programs (harness/wasm32.py, the train's wasm32
        # job): wasmtime for wasm32-wasip1 and `--invoke` of a library's exports,
        # wasm-tools where the harnesses read objects, wasm-bindgen and node for web guests. The
        # targets are rustup's: `rustup target add wasm32-wasip1 wasm32-unknown-unknown`.
        devShells.wasm = pkgs.mkShell (llvmEnv // {
          packages = [ pkgs.rustup pkgs.python3 llvm pkgs.wasmtime pkgs.wasm-tools pkgs.wasm-bindgen-cli pkgs.nodejs ];
          RUSTUP_TOOLCHAIN = pkgs.lib.fileContents ./harness/toolchain;
          hardeningDisable = [ "all" ];
        });
        devShells.bench = pkgs.mkShell bench;
        # What harness/bench.py and runbench.py measure bun in (bunprep.py, docs/research/bun.md):
        # bun's own pinned nightly in place of blaze's (rust-src, which blaze reads `core` from), and
        # what bun's own build of its C and C++ needs, as its flake.nix has it: LLVM 23 as the
        # C and C++ compiler, cmake, ninja, nasm (x86), go, a bun to run its build scripts, and on Linux lld.
        # blaze itself is built in the bench shell.
        devShells.bench-bun = (pkgs.mkShell.override { stdenv = pkgs.llvmPackages_23.stdenv; }) {
          packages = [
            (pkgs.rust-bin.nightly."2026-09-15".default.override { extensions = [ "rust-src" ]; })
            pkgs.python3
            pkgs.git
            pkgs.curl
            pkgs.unzip
            pkgs.xz
            pkgs.pkg-config
            pkgs.cmake
            pkgs.ninja
            pkgs.nasm
            pkgs.go
            pkgs.bun
            pkgs.perl
            pkgs.ruby
            pkgs.libtool
            pkgs.llvmPackages_23.clang
            pkgs.llvmPackages_23.llvm
            pkgs.llvmPackages_23.lld
          ];
          hardeningDisable = [ "all" ];
          shellHook = ''
            export CC=${pkgs.llvmPackages_23.clang}/bin/clang CXX=${pkgs.llvmPackages_23.clang}/bin/clang++
            export AR=${pkgs.llvmPackages_23.llvm}/bin/llvm-ar RANLIB=${pkgs.llvmPackages_23.llvm}/bin/llvm-ranlib
          '' + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            export NIX_CFLAGS_LINK="''${NIX_CFLAGS_LINK:+$NIX_CFLAGS_LINK }-fuse-ld=lld"
          '';
        };
        # The bench shell and what zed's build needs besides (its own nix/build.nix lists the
        # same): `nix develop .#bench-zed`.
        devShells.bench-zed = pkgs.mkShell (bench // {
          packages = bench.packages ++ [ pkgs.cmake pkgs.protobuf ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.curl
            pkgs.fontconfig
            pkgs.freetype
            pkgs.libgit2
            pkgs.sqlite
            pkgs.zstd
            pkgs.glib
            pkgs.libva
            pkgs.vulkan-loader
            pkgs.libglvnd
            pkgs.libx11
            pkgs.libxcb
            pkgs.libdrm
            pkgs.libgbm
            pkgs.libxcomposite
            pkgs.libxdamage
            pkgs.libxext
            pkgs.libxfixes
            pkgs.libxrandr
          ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # zed's build script looks for glib's headers under /usr/include.
          CXXFLAGS = "-I${pkgs.glib.dev}/include/glib-2.0 -I${pkgs.glib.out}/lib/glib-2.0/include";
          ZSTD_SYS_USE_PKG_CONFIG = "1";
        } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          # The WebRTC zed links statically, the build its build script would download into
          # every cold target directory.
          LK_CUSTOM_WEBRTC = pkgs.fetchzip {
            url = "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-linux-x64-release.zip";
            sha256 = "0hlv1p6fi1lgfdyq8q49gghbqbqnq50icrw7i25g2qwqmpf2jyi7";
          };
        });
      });
}
