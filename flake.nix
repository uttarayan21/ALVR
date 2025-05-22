{
  description = "Flake utils demo";

  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.self.submodules = true;

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages = rec {
          alvr = pkgs.callPackage ./alvr.nix {};
          default = alvr;
        };
        devShells = rec {
          defeault = pkgs.mkShell {
            packages = [
              pkgs.rust
              pkgs.cargo
            ];
          };
        };
      }
    );
}
