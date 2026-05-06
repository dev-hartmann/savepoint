{
  description = "A command watcher that commits when you fix errors";

  inputs = {
    flakebox = {
      url = "github:rustshop/flakebox?rev=62af969ab344229d2a0d585a482293b3f186b221";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      flake-utils,
      flakebox,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        projectName = "savepoint";
        pkgs = flakebox.inputs.nixpkgs.legacyPackages.${system};

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [ ".#ci.${projectName}" ];
            git.pre-commit.enable = false;
            github.ci.enable = false;
            github.ci.workflows.flakebox-flakehub-publish.enable = false;
            convco.commit-msg.enable = false;
          };
        };

        buildPaths = [
          "Cargo.toml"
          "Cargo.lock"
          "src"
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
        };

        multiBuild = (flakeboxLib.craneMultiBuild { }) (
          craneLib':
          let
            craneLib = (
              craneLib'.overrideArgs {
                pname = projectName;
                src = buildSrc;
                nativeBuildInputs = [ ];
              }
            );
          in
          {
            ${projectName} = craneLib.buildPackage { };
          }
        );
      in
      {
        packages.default = multiBuild.${projectName};

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells { };
      }
    );
}
