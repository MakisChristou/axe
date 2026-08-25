{
  perSystem =
    {
      pkgs,
      rustToolchain,
      ...
    }:
    let
      devTools = with pkgs; [
        git
        # scripts/*.sh drive the Axelar CLIs through jq
        jq

        pkg-config
        openssl

        cargo-audit
      ];

      envs = {
        OPENSSL_NO_VENDOR = 1;
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        OPENSSL_DIR = "${pkgs.openssl.dev}";
      };

      devInfo = ''
        echo "axe development environment loaded!"
        echo "Rust version: $(rustc --version)"
        echo "Cargo version: $(cargo --version)"
        echo ""
      '';
    in
    {
      devShells.default = pkgs.mkShell {
        packages = [ rustToolchain ] ++ devTools;

        inherit (envs)
          OPENSSL_NO_VENDOR
          OPENSSL_LIB_DIR
          OPENSSL_DIR
          ;

        RUST_BACKTRACE = "0";

        # rustc defaults to its bundled `rust-lld` on x86_64-linux, which
        # skips nixpkgs' ld wrapper and so leaves the RPATH empty. Every
        # binary that links openssl (reqwest -> native-tls) then dies at
        # startup on `libssl.so.3` -- including the test binaries `cargo
        # test` runs. Fall back to the wrapped system linker.
        RUSTFLAGS = "-C linker-features=-lld";

        # Opt users entering from this checkout into its git hooks
        # (fmt + clippy + tests on commit and push, see .githooks/).
        shellHook = ''
          axe_repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
          if [ -n "$axe_repo_root" ] && [ -x "$axe_repo_root/.githooks/pre-commit" ]; then
            git -C "$axe_repo_root" config --local core.hooksPath .githooks
          fi

          ${devInfo}
        '';
      };
    };
}
