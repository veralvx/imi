# imi's flake: the toolchain rust-toolchain.toml pins -- rust-overlay reads that file, so the MSRV
# is written once and rustup and Nix build with the same compiler -- the tools `just checks`, the
# loop-device suites and `just release` run, and CI's checks as derivations.
#
#   nix develop          the shell: the toolchain, just, dprint, cog, git-cliff, cargo-audit...
#   nix develop .#miri   nightly with Miri, for `just miri` (`cargo miri test`)
#   nix fmt              nixfmt, through treefmt; `nix fmt -- --ci` formats, and fails if that
#                        changed a file
#   nix flake check      clippy, rustfmt, rustdoc and the tests, and the format (`nix fmt`'s
#                        verdict), offline in the sandbox; CI runs it on x86_64 and aarch64
#                        (.github/workflows/nix.yml)
#   nix build            the binary, as ./result/bin/imi
#   nix run . -- --help  the binary, run
#
# Linux only: imi is a Linux block-device flasher (O_EXCL, O_DIRECT, BLKRRPART, sysfs), and
# builds for nothing else. The tests the sandbox runs are the unprivileged ones; the loop-device
# suites stay `#[ignore]`d (AGENTS.md, "Testing additions"): they need root and /dev/loop*.
#
# flake.lock pins nixpkgs and rust-overlay: commit it; `nix flake update` moves it.
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      # version, licence and repository from the manifests, so a release's bump is the flake's
      manifest = lib.importTOML ./Cargo.toml;
      inherit (manifest.workspace.package) version license repository;
      cargoPackage = (lib.importTOML ./crates/imi/Cargo.toml).package;
      pname = cargoPackage.name;

      # what a build reads: the repository less its tooling and contributor docs, so editing
      # those rebuilds nothing, while a new crate, build.rs or test is in without a word;
      # README.md stays, as api_consumer.rs compares its example against the compiled copy
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.difference ./. (
          lib.fileset.unions (
            map lib.fileset.maybeMissing [
              ./.agents
              ./.github
              ./.gitignore
              ./AGENTS.md
              ./CHANGELOG.md
              ./CODE_OF_CONDUCT.md
              ./CONTRIBUTING.md
              ./SECURITY.md
              ./cliff.toml
              ./cog.toml
              ./dist-workspace.toml
              ./dprint.json
              ./flake.lock
              ./flake.nix
              ./justfile
            ]
          )
        );
      };

      perSystem =
        pkgs:
        let
          rust-bin = rust-overlay.lib.mkRustBin { } pkgs;
          toolchain = rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          # `nix fmt`: the Nix. The Rust is rustfmt's (`just fmt`), the Markdown and TOML dprint's.
          formatter = pkgs.treefmt.withConfig {
            settings = {
              on-unmatched = "info";
              formatter.nixfmt = {
                command = "nixfmt";
                includes = [ "*.nix" ];
              };
            };
            runtimeInputs = [ pkgs.nixfmt ];
          };
          common = {
            inherit version src;
            cargoLock.lockFile = ./Cargo.lock;
          };
          # a check: one cargo command, offline against the vendored lock, a stamp for output
          cargoCheck =
            name: command:
            rustPlatform.buildRustPackage (
              common
              // {
                pname = "${pname}-${name}";
                buildPhase = ''
                  runHook preBuild
                  ${command}
                  runHook postBuild
                '';
                doCheck = false;
                installPhase = ''
                  runHook preInstall
                  touch $out
                  runHook postInstall
                '';
              }
            );
          # the release build (Cargo.toml's profile: fat LTO, panic = "unwind"), the workspace's
          # tests run by buildRustPackage's checkPhase
          package = rustPlatform.buildRustPackage (
            common
            // {
              inherit pname;
              cargoBuildFlags = [ "--package=${pname}" ];
              cargoTestFlags = [ "--workspace" ];
              # the sandbox has no /sys, and these tests probe the host's real one by design -- the
              # live device listing, and the sysfs wrappers against a direct probe: `cargo test`
              # and CI run them, against a host. By exact name, so a new test is never caught.
              checkFlags = [
                "--exact"
                "--skip=picker::tests::live_list_renders_one_line_each"
                "--skip=omitting_dev_without_a_terminal_refuses_with_guidance"
                "--skip=common::sysfs::tests::public_wrappers_match_a_direct_probe_of_the_real_sysfs"
                "--skip=common::sysfs::tests::public_wrappers_resolve_against_the_real_sysfs"
                "--skip=live_candidates_are_structurally_sound"
                "--skip=no_candidate_carries_the_root_filesystem"
              ];
              meta = {
                inherit (cargoPackage) description;
                homepage = repository;
                # the SPDX expression's licence identifiers: its operators, parentheses and
                # `WITH` exceptions aside
                license = map lib.getLicenseFromSpdxId (
                  lib.subtractLists [ "" "AND" "OR" ] (
                    lib.filter lib.isString (builtins.split "[ ()]+|WITH [^ ()]+" license)
                  )
                );
                platforms = lib.platforms.linux;
                mainProgram = pname;
              };
            }
          );
        in
        {
          packages.default = package;
          checks = {
            clippy = cargoCheck "clippy" "cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings";
            fmt = cargoCheck "fmt" "cargo fmt --all -- --check";
            doc = cargoCheck "doc" "RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --all-features --locked --offline";
            # the formatter's verdict on the whole tree: what `nix fmt` would change fails it
            format = formatter.check ./.;
            inherit package;
          };
          devShells = {
            default = pkgs.mkShell {
              packages = [
                toolchain
                pkgs.just
                pkgs.jq
                pkgs.dprint
                pkgs.cocogitto
                pkgs.git-cliff
                pkgs.cargo-audit
                pkgs.cargo-dist
                pkgs.cargo-mutants
                pkgs.nixfmt
                pkgs.valgrind
                # what the root-gated loop-device suites shell out to build their fixtures:
                # losetup, mkswap, swapon, mount (util-linux) and mkfs.ext4 (e2fsprogs)
                pkgs.util-linux
                pkgs.e2fsprogs
                # .agents/ptycap.py, which captures the progress bars under a pseudo-terminal
                pkgs.python3
              ];
            };
            # Miri runs on nightly only: rust-overlay's latest nightly that ships it
            miri = pkgs.mkShell {
              packages = [
                (rust-bin.selectLatestNightlyWith (
                  nightly:
                  nightly.minimal.override {
                    extensions = [
                      "miri"
                      "rust-src"
                    ];
                  }
                ))
              ];
            };
          };
          inherit formatter;
        };

      bySystem = lib.genAttrs systems (system: perSystem nixpkgs.legacyPackages.${system});
      output = name: lib.mapAttrs (_: outputs: outputs.${name}) bySystem;
    in
    {
      packages = output "packages";
      checks = output "checks";
      devShells = output "devShells";
      formatter = output "formatter";
    };
}
