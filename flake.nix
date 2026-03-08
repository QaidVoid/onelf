{
  description = "Onelf flake";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            gcc
            musl.dev
            pkg-config
          ];

          shellHook = ''
            export CC_x86_64_unknown_linux_musl=musl-gcc

            # NixOS + rustup fix: rustc passes -nodefaultlibs to gcc, which
            # prevents nix's gcc wrapper from adding -L for glibc. Add it
            # explicitly so -lc resolves when linking build scripts.
            _linker_wrap=$(mktemp -d)/host-gcc
            printf '#!/bin/sh\nexec gcc -L${pkgs.glibc}/lib "$@"\n' > "$_linker_wrap"
            chmod +x "$_linker_wrap"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$_linker_wrap"
          '';
        };
      }
    );
}
