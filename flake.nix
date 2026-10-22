{
  description = "Phonebridge: use your Android phone as a microphone and webcam on Linux";

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
        phonebridge = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
        default = phonebridge;
      });

      devShells = forAll (system: {
        default = import ./shell.nix { pkgs = nixpkgs.legacyPackages.${system}; };
      });

      # Installs the client and, for the webcam, a v4l2loopback device labelled
      # "Phonebridge" (found automatically by `phonebridge --camera`).
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.phonebridge;
        in
        {
          options.programs.phonebridge = {
            enable = lib.mkEnableOption "Phonebridge (Android phone as microphone and webcam)";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "phonebridge.packages.\${system}.default";
              description = "The phonebridge package to install.";
            };
            camera.enable = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Load v4l2loopback so the phone's camera can appear as a webcam.";
            };
            camera.videoNr = lib.mkOption {
              type = lib.types.int;
              default = 10;
              description = "Number N of the webcam device /dev/videoN.";
            };
          };

          config = lib.mkIf cfg.enable (
            lib.mkMerge [
              { environment.systemPackages = [ cfg.package ]; }
              (lib.mkIf cfg.camera.enable {
                boot.extraModulePackages = [ config.boot.kernelPackages.v4l2loopback ];
                boot.kernelModules = [ "v4l2loopback" ];
                # exclusive_caps: Chrome only lists the device as a camera while it
                # has a writer; `phonebridge --camera` keeps one attached for its
                # whole run (placeholder frames until the phone streams).
                boot.extraModprobeConfig = ''
                  options v4l2loopback devices=1 video_nr=${toString cfg.camera.videoNr} card_label="Phonebridge" exclusive_caps=1
                '';
                # boot.kernelModules only covers the booted system's module tree, so
                # after a `nixos-rebuild switch` that adds v4l2loopback it would need
                # a reboot. Load it from the new system's tree instead.
                systemd.services.phonebridge-v4l2loopback = {
                  description = "Load v4l2loopback for the Phonebridge webcam";
                  wantedBy = [ "multi-user.target" ];
                  after = [ "systemd-modules-load.service" ];
                  serviceConfig = {
                    Type = "oneshot";
                    RemainAfterExit = true;
                  };
                  script = ''
                    [ -e /sys/module/v4l2loopback ] ||
                      ${pkgs.kmod}/bin/modprobe -d /run/current-system/kernel-modules v4l2loopback
                  '';
                };
              })
            ]
          );
        };
    };
}
