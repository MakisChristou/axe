{ inputs, ... }:
{
  imports = [ inputs.treefmt-nix.flakeModule ];

  perSystem =
    {
      rustToolchain,
      ...
    }:
    {
      treefmt = {
        projectRootFile = "flake.nix";

        programs = {
          nixfmt.enable = true;

          shfmt = {
            enable = true;
            indent_size = 4;
          };

          taplo = {
            enable = true;
            settings = {
              formatting.reorder_keys = false;
              rule = [
                {
                  include = [ "**/Cargo.toml" ];
                  keys = [
                    "dependencies"
                    "dev-dependencies"
                    "build-dependencies"
                  ];
                  formatting.reorder_keys = true;
                }
              ];
            };
          };

          rustfmt = {
            enable = true;
            package = rustToolchain;
          };

          # Configure Prettier to handle Markdown and YAML
          prettier = {
            enable = true;
            includes = [
              "*.md"
              "*.yaml"
              "*.yml"
            ];
          };
        };
      };
    };
}
