{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = [ pkgs.python3 pkgs.android-tools pkgs.libopus pkgs.pipewire ];
  LIBOPUS = "${pkgs.libopus}/lib/libopus.so.0";
}
