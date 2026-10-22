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
  pname = "wo-mic-linux";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../assets/womic.png
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
    wrapProgram $out/bin/womic \
      --prefix PATH : ${lib.makeBinPath [ android-tools pipewire libnotify ]} \
      --set-default WOMIC_ICON $out/share/icons/hicolor/512x512/apps/wo-mic.png
    install -Dm644 assets/womic.png $out/share/icons/hicolor/512x512/apps/wo-mic.png
    # Noctalia bar toggle: point a `path` plugin source at share/noctalia-plugins.
    mkdir -p $out/share/noctalia-plugins
    cp -r noctalia/womic $out/share/noctalia-plugins/womic
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "wo-mic";
      desktopName = "WO Mic";
      comment = "Use your Android phone as a microphone (toggle)";
      exec = "womic toggle";
      icon = "wo-mic";
      categories = [
        "AudioVideo"
        "Audio"
      ];
      keywords = [
        "microphone"
        "android"
        "womic"
      ];
    })
  ];

  meta = {
    description = "WO Mic client for Linux (USB via adb, PipeWire virtual microphone)";
    homepage = "https://github.com/wellington-vell/wo-mic-linux";
    license = lib.licenses.mit;
    mainProgram = "womic";
    platforms = lib.platforms.linux;
  };
}
