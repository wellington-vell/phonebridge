{
  lib,
  stdenvNoCC,
  makeWrapper,
  makeDesktopItem,
  copyDesktopItems,
  python3,
  android-tools,
  pipewire,
  libopus,
  libnotify,
  coreutils,
  procps,
  gnugrep,
}:
stdenvNoCC.mkDerivation {
  pname = "wo-mic-linux";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../womic.py
      ./womic-toggle.sh
      ../assets/womic.png
      ../noctalia
    ];
  };

  nativeBuildInputs = [
    makeWrapper
    copyDesktopItems
  ];

  installPhase = ''
    runHook preInstall
    install -Dm755 womic.py $out/libexec/womic.py
    makeWrapper ${python3}/bin/python3 $out/bin/womic \
      --add-flags $out/libexec/womic.py \
      --prefix PATH : ${lib.makeBinPath [ android-tools pipewire ]} \
      --set-default LIBOPUS ${libopus}/lib/libopus.so.0
    install -Dm755 nix/womic-toggle.sh $out/bin/womic-toggle
    wrapProgram $out/bin/womic-toggle \
      --prefix PATH : ${lib.makeBinPath [ libnotify coreutils procps gnugrep ]}:$out/bin \
      --set-default WOMIC_ICON $out/share/icons/hicolor/512x512/apps/wo-mic.png
    install -Dm644 assets/womic.png $out/share/icons/hicolor/512x512/apps/wo-mic.png
    # Noctalia bar toggle: point a `path` plugin source at share/noctalia-plugins.
    mkdir -p $out/share/noctalia-plugins
    cp -r noctalia/womic $out/share/noctalia-plugins/womic
    runHook postInstall
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "wo-mic";
      desktopName = "WO Mic";
      comment = "Use your Android phone as a microphone (toggle)";
      exec = "womic-toggle";
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
