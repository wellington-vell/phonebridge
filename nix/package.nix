{
  lib,
  rustPlatform,
  makeWrapper,
  makeDesktopItem,
  copyDesktopItems,
  android-tools,
  pipewire,
  libopus,
  libnotify,
}:
rustPlatform.buildRustPackage {
  pname = "wemic";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../assets/wemic.png
      ../noctalia
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    makeWrapper
    copyDesktopItems
  ];
  buildInputs = [ libopus ];

  postInstall = ''
    wrapProgram $out/bin/wemic \
      --prefix PATH : ${lib.makeBinPath [ android-tools pipewire libnotify ]} \
      --set-default WEMIC_ICON $out/share/icons/hicolor/512x512/apps/wemic.png
    install -Dm644 assets/wemic.png $out/share/icons/hicolor/512x512/apps/wemic.png
    # Noctalia bar toggle: point a `path` plugin source at share/noctalia-plugins.
    mkdir -p $out/share/noctalia-plugins
    cp -r noctalia/wemic $out/share/noctalia-plugins/wemic
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "wemic";
      desktopName = "WeMic";
      comment = "Use your Android phone as a microphone (toggle)";
      exec = "wemic toggle";
      icon = "wemic";
      categories = [
        "AudioVideo"
        "Audio"
      ];
      keywords = [
        "microphone"
        "android"
        "wemic"
      ];
    })
  ];

  meta = {
    description = "WeMic client for Linux (USB, Wi-Fi or Bluetooth; PipeWire virtual microphone)";
    homepage = "https://github.com/wellington-vell/wo-mic-linux";
    license = lib.licenses.mit;
    mainProgram = "wemic";
    platforms = lib.platforms.linux;
  };
}
