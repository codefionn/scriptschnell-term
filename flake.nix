{
  description = "scriptschnell-term: GPU-rendered terminal and browser panels organized by project and worktree";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # rust-toolchain.toml pins a Rust release that nixpkgs may not carry yet.
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
      inherit (nixpkgs) lib;
      # Linux only: CI builds and tests the app on Linux, and the macOS job is off.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f (pkgsFor system));
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      packageFor =
        pkgs:
        pkgs.callPackage ./nix/package.nix {
          rustToolchain = toolchainFor pkgs;
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        scriptschnell-term = packageFor pkgs;
        default = scriptschnell-term;
      });

      apps = forAllSystems (
        pkgs:
        let
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          app = name: description: {
            type = "app";
            program = "${pkg}/bin/${name}";
            meta = { inherit description; };
          };
        in
        {
          default = self.apps.${pkgs.stdenv.hostPlatform.system}.scriptschnell-term;
          scriptschnell-term = app "scriptschnell-term" pkg.meta.description;
          scriptschnell-term-remote = app "scriptschnell-term-remote" "Remote machine server, client and VMs for scriptschnell-term";
          scriptschnell-term-web = app "scriptschnell-term-web" "Web access to scriptschnell-term machines from browsers and phones";
        }
      );

      # Adds pkgs.scriptschnell-term, built with the toolchain from rust-toolchain.toml.
      overlays.default = lib.composeManyExtensions [
        rust-overlay.overlays.default
        (final: _prev: { scriptschnell-term = packageFor final; })
      ];

      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/shell.nix {
          rustToolchain = (toolchainFor pkgs).override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
          scriptschnell-term = packageFor pkgs;
        };
      });

      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
