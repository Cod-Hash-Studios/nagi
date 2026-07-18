{
  description = "nagi — terminal workspace manager for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      rustToolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatformFor =
        pkgs:
        let
          rustToolchain = rustToolchainFor pkgs;
        in
        pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          nagi = pkgs.callPackage ./nix/package.nix {
            rustPlatform = rustPlatformFor pkgs;
          };
        in
        {
          inherit nagi;
          default = nagi;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/nagi";
          meta.description = "Run Nagi";
        };
      });

      checks = forAllSystems (system: {
        nagi = self.packages.${system}.default;
        default = self.checks.${system}.nagi;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor pkgs;
        in
        {
          default = pkgs.mkShell {
            name = "nagi-dev";
            packages = with pkgs; [
              cargo-nextest
              cmake
              just
              ninja
              pkg-config
              rustToolchain
              zig_0_15
            ];

            env = {
              LIBGHOSTTY_VT_OPTIMIZE = "Debug";
              LIBGHOSTTY_VT_SIMD = "true";
            };
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);

      overlays.default = lib.composeExtensions rust-overlay.overlays.default (
        final: _prev: {
          nagi = final.callPackage ./nix/package.nix {
            rustPlatform = rustPlatformFor final;
          };
        }
      );
    };
}
