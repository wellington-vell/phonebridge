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
  ffmpeg-headless,
  dejavu_fonts,
}:
rustPlatform.buildRustPackage {
  pname = "phonebridge";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../models
      ../assets/phonebridge.png
      ../assets/phonebridge.svg
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
    wrapProgram $out/bin/phonebridge \
      --prefix PATH : ${lib.makeBinPath [
        android-tools
        pipewire
        libnotify
        ffmpeg-headless
      ]} \
      --set-default PHONEBRIDGE_ICON $out/share/icons/hicolor/512x512/apps/phonebridge.png \
      --set-default PHONEBRIDGE_FONT ${dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf
    install -Dm644 assets/phonebridge.png $out/share/icons/hicolor/512x512/apps/phonebridge.png
    install -Dm644 assets/phonebridge.svg $out/share/icons/hicolor/scalable/apps/phonebridge.svg
    # Noctalia bar toggle: point a `path` plugin source at share/noctalia-plugins.
    mkdir -p $out/share/noctalia-plugins
    cp -r noctalia/phonebridge $out/share/noctalia-plugins/phonebridge
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "phonebridge";
      desktopName = "Phonebridge";
      comment = "Use your Android phone as a microphone and webcam (toggle)";
      exec = "phonebridge toggle";
      icon = "phonebridge";
      categories = [
        "AudioVideo"
        "Audio"
        "Video"
      ];
      keywords = [
        "microphone"
        "webcam"
        "camera"
        "android"
        "phonebridge"
      ];
    })
  ];

  meta = {
    description = "Use your Android phone as a microphone and webcam on Linux (PipeWire virtual mic, v4l2loopback webcam)";
    homepage = "https://github.com/wellington-vell/phonebridge";
    license = lib.licenses.mit;
    mainProgram = "phonebridge";
    platforms = lib.platforms.linux;
  };
}
