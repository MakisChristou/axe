{
  description = "axe — Swiss army knife CLI for Axelar cross-chain development";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, nixpkgs, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        ./nix/devshell.nix
        ./nix/formatting.nix
        ./nix/package.nix
      ];

      perSystem =
        { system, ... }:
        let
          pkgs = import nixpkgs { inherit system; };

          # rust-toolchain.toml is the source of truth for the component
          # list. `fenix.stable` itself is pinned by the fenix flake input,
          # so the toolchain only moves on `nix flake update` — no
          # per-toolchain sha256 to babysit.
          toolchain = (pkgs.lib.importTOML ./rust-toolchain.toml).toolchain;

          rustToolchain =
            assert pkgs.lib.assertMsg (toolchain.channel == "stable")
              "rust-toolchain.toml pins the '${toolchain.channel}' channel, but this flake only wires up fenix's stable channel.";
            inputs.fenix.packages.${system}.stable.withComponents toolchain.components;
        in
        {
          _module.args = {
            inherit pkgs rustToolchain;
          };
        };
    };
}
