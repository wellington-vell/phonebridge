{
  description = "WO Mic client for Linux";

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
        wo-mic-linux = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
        default = wo-mic-linux;
      });

      devShells = forAll (system: {
        default = import ./shell.nix { pkgs = nixpkgs.legacyPackages.${system}; };
      });
    };
}
