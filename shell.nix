{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.android-tools pkgs.libopus pkgs.pipewire ];
}
