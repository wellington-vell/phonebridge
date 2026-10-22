{
  description = "WeMic client for Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAll = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAll (system: rec {
        wemic = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
        default = wemic;
      });

      devShells = forAll (system: {
        default = import ./shell.nix { pkgs = nixpkgs.legacyPackages.${system}; };
      });
    };
}
