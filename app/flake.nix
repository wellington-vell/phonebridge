{
  description = "WeMic for Android (Tauri) — development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config = {
          allowUnfree = true;
          android_sdk.accept_license = true;
        };
      };
      buildToolsVersion = "36.0.0";
      ndkVersion = "29.0.14206865";
      android = pkgs.androidenv.composeAndroidPackages {
        platformVersions = [
          "34"
          "35"
          "36"
        ];
        buildToolsVersions = [
          "34.0.0"
          "35.0.0"
          buildToolsVersion
        ];
        includeNDK = true;
        ndkVersions = [ ndkVersion ];
        cmakeVersions = [ "3.31.6" ];
        includeEmulator = false;
        includeSystemImages = false;
      };
      sdk = "${android.androidsdk}/libexec/android-sdk";
      rust = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "rust-src"
          "rust-analyzer"
        ];
        targets = [
          "aarch64-linux-android"
          "armv7-linux-androideabi"
          "x86_64-linux-android"
          "i686-linux-android"
        ];
      };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          rust
          pkgs.cargo-tauri
          pkgs.jdk17
          pkgs.cmake
          pkgs.pkg-config
          pkgs.android-tools
          # desktop dev build (`cargo tauri dev`)
          pkgs.webkitgtk_4_1
          pkgs.gtk3
          pkgs.librsvg
          pkgs.openssl
          pkgs.libsoup_3
        ];
        ANDROID_HOME = sdk;
        ANDROID_SDK_ROOT = sdk;
        NDK_HOME = "${sdk}/ndk/${ndkVersion}";
        ANDROID_NDK = "${sdk}/ndk/${ndkVersion}";
        JAVA_HOME = pkgs.jdk17.home;
        # The aapt2 Gradle downloads from Maven is not runnable on NixOS.
        GRADLE_OPTS = "-Dorg.gradle.project.android.aapt2FromMavenOverride=${sdk}/build-tools/${buildToolsVersion}/aapt2";
      };
    };
}
