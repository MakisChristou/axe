{
  description = "axe — Swiss army knife CLI for Axelar cross-chain development";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          axe = pkgs.rustPlatform.buildRustPackage {
            pname = "axe";
            inherit version;
            src = self;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # All four crates come from the same pinned xrpl-sdk-rust checkout.
              outputHashes = {
                "xrpl_api-0.16.6" = "sha256-oacgaU/4EHS8LlDOihTsCe5GzyXf2zpb07MItJaH4W0=";
                "xrpl_binary_codec-0.16.6" = "sha256-oacgaU/4EHS8LlDOihTsCe5GzyXf2zpb07MItJaH4W0=";
                "xrpl_http_client-0.16.6" = "sha256-oacgaU/4EHS8LlDOihTsCe5GzyXf2zpb07MItJaH4W0=";
                "xrpl_types-0.16.6" = "sha256-oacgaU/4EHS8LlDOihTsCe5GzyXf2zpb07MItJaH4W0=";
              };
            };

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.openssl ];

            meta = {
              description = "Swiss army knife CLI for Axelar cross-chain development: deploy, test, decode, monitor";
              homepage = "https://github.com/axelarnetwork/axe";
              mainProgram = "axe";
            };
          };
          default = axe;
        }
      );

      overlays.default = final: prev: {
        axe = self.packages.${final.stdenv.hostPlatform.system}.axe;
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              git

              pkg-config
              openssl
            ];

            # Opt users entering from this checkout into its git hooks
            # (fmt + clippy + tests on commit and push, see .githooks/).
            shellHook = ''
              axe_repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
              if [ -n "$axe_repo_root" ] && [ -x "$axe_repo_root/.githooks/pre-commit" ]; then
                git -C "$axe_repo_root" config --local core.hooksPath .githooks
              fi
            '';
          };
        }
      );
    };
}
