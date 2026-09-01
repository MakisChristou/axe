{ inputs, withSystem, ... }:
{
  flake.overlays.default = _final: prev: {
    axe = withSystem prev.stdenv.hostPlatform.system ({ config, ... }: config.packages.axe);
  };

  perSystem =
    {
      config,
      pkgs,
      ...
    }:
    let
      version = (pkgs.lib.importTOML ../Cargo.toml).package.version;
    in
    {
      # Deliberately nixpkgs' `rustPlatform`, not the fenix toolchain the
      # devshell uses. fenix ships upstream rustc, which links without
      # going through nixpkgs' cc/ld wrapper, so the binary comes out with
      # an empty RPATH and dies at startup on `libssl.so.3`. `nix develop`
      # gets the pinned toolchain; `nix build` gets a runnable binary.
      packages.axe = pkgs.rustPlatform.buildRustPackage {
        pname = "axe";
        inherit version;
        src = inputs.self;

        cargoLock = {
          lockFile = ../Cargo.lock;
          # All four crates come from the same pinned xrpl-sdk-rust checkout.
          outputHashes = {
            "xrpl_api-0.16.6" = "sha256-oacgaU/4EHS8LlDOihTsCe5GzyXf2zpb07MItJaH4W0=";
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

      packages.default = config.packages.axe;
    };
}
