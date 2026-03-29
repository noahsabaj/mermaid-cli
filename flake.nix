{
  description = "A minimal development shell for cargo2nix (Phase 1: uses pre-built cargo)";

  inputs = {
    nixpkgs.url = "github:meta-introspector/nixpkgs?ref=feature/CRQ-016-nixify";
    rust-overlay = {
      url = "github:meta-introspector/rust-overlay?ref=feature/CRQ-016-nixify";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:meta-introspector/flake-utils?ref=feature/CRQ-016-nixify";
    #cargo2nix.url = "github:cargo2nix/cargo2nix/release-0.12";
  };

  outputs =
    { self
    , nixpkgs
    , rust-overlay
    , flake-utils
    , #cargo2nix
    }:
    flake-utils.lib.eachDefaultSystem (system:
    let
      overlays = [
        #cargo2nix.overlays.default
        rust-overlay.overlays.default
      ];
      pkgs = import nixpkgs {
        inherit system overlays;
        config = {
          permittedInsecurePackages = [ "openssl-1.1.1w" ];
        };
      };

      myRustc = pkgs.rust-bin.nightly."2025-09-16".default;

      rustPkgs = pkgs.rustBuilder.makePackageSet {
        rustToolchain = myRustc;
      };

      # Use pre-built cargo from nixpkgs for Phase 1
      cargo = pkgs.cargo;

      workspaceShell = pkgs.mkShell {
        packages = [
          pkgs.statix
          pkgs.openssl_1_1.dev
          pkgs.zlib.dev
          pkgs.sccache
          pkgs.pkg-config
        ];
        shellHook = ''
          export PKG_CONFIG_PATH=${pkgs.openssl_1_1.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig:$PKG_CONFIG_PATH
          export PATH=${myRustc}/bin:${cargo}/bin:${pkgs.sccache}/bin:$PATH

          echo "🦀 Rust development environment ready!"
        '';
      };
    in
    rec {
      devShells = {
        default = workspaceShell;
      };

      packages = rec {
        inherit cargo;
        default = cargo;
      };

      apps = rec {
        cargo = { type = "app"; program = "${packages.cargo}/bin/cargo"; };
        default = cargo;
      };
    }
    );
}
