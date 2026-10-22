{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer pkgs.android-tools pkgs.libopus pkgs.pipewire ];
  # Standard library sources for rust-analyzer.
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
