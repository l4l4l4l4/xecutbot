#!/usr/bin/env bash
set -euo pipefail
revision="$(nix eval --impure --raw --expr '(builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked.rev')"
exec nix-shell -I "nixpkgs=https://github.com/NixOS/nixpkgs/archive/${revision}.tar.gz" \
  -p pkg-config openssl gnumake cargo rustc rustfmt clippy python3 \
  --run 'env CARGO_BUILD_JOBS=2 make smoke'
