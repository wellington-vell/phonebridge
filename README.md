# Phonebridge

Use your Android phone as a **microphone and webcam** on Linux. The client
speaks the [WO Mic](https://wolicheng.com/womic) audio protocol over **USB**
(through `adb`), **Wi-Fi** or **Bluetooth** and exposes the phone as a PipeWire
virtual microphone named **"Phonebridge"**. With `--camera` it also streams the
phone's camera (H.264, USB or Wi-Fi) into a v4l2loopback **webcam**.

On the phone, use the [Phonebridge Android app](#android-app) in `app/` (Tauri),
which adds camera sharing, a volume slider, a mute button, audio source
selection and keep-screen-on. The official WO Mic app works for audio only.

The protocol was reverse engineered from the official Windows client
(`WOMicClient.exe` 6.3). No Windows code is included or needed.

## Requirements

- Linux with PipeWire (`pw-loopback`, `pw-cat`)
- `adb` (Android platform tools) for USB
- BlueZ for Bluetooth
- `libopus`
- For the webcam: `ffmpeg` and the `v4l2loopback` kernel module
- Rust toolchain (to build without Nix)
- On the phone: the Phonebridge app in `app/`, or the official WO Mic app

With Nix all of these are provided for you.

## Usage

1. On the phone: enable **Developer options → USB debugging**, plug it in and
   accept the authorization prompt.
2. In the phone app select **USB** (official WO Mic: **Settings → Transport →
   USB**), then press **Start**.
3. On the PC:

   ```sh
   nix run github:wellington-vell/phonebridge
   # or, without Nix (needs libopus development files)
   cargo run --release
   ```

4. Select **Phonebridge** as the input device in your apps (or `wpctl set-default`).

Stop with `Ctrl+C`. The client reconnects automatically if the phone app stops
or the cable is unplugged: over USB it redoes `adb forward` before every
connection attempt (forwards vanish when the phone is replugged or the adb
server restarts), and it starts fine without a phone attached, waiting for one.

### Pairing code

The Phonebridge app shows a **pairing code** (`ABCDE-FGHJK-MNPQR-STVWX`). Give it
to the client and only that phone is accepted, and audio and video are
encrypted (AES-256-GCM, see [docs/secure-channel.md](docs/secure-channel.md)):

```sh
phonebridge --token ABCDE-FGHJK-MNPQR-STVWX
```

Case, dashes and spaces don't matter. Rather than typing it each time, store it
in `$XDG_CONFIG_HOME/phonebridge/token` (`~/.config/phonebridge/token`), which
must be yours and private, or set `PHONEBRIDGE_TOKEN`:

```sh
echo ABCDE-FGHJK-MNPQR-STVWX | install -Dm600 /dev/stdin ~/.config/phonebridge/token
```

`--token` wins over `PHONEBRIDGE_TOKEN`, which wins over the file. Background
instances (`phonebridge start --token …`) get the code through their
environment, so it doesn't show up in `ps`. Without a code the client speaks the
original plaintext protocol: it works with the official WO Mic app, but anyone
on the network can listen or pose as the phone, and it says so in the log.

### Wi-Fi

Phone and PC on the same network; select **Wi-Fi** in the phone app and use the
address it shows:

```sh
phonebridge --wifi 192.168.1.23
```

Media comes over the same TCP connection by default. `--udp PORT` asks the phone
to send it as UDP datagrams to that local port instead (open it in your firewall);
the port listens only on the address that reaches the phone.

### Bluetooth

Pair the phone with the PC once (`bluetoothctl`, or your desktop's settings),
select **Bluetooth** in the phone app, then use the phone's address from
`bluetoothctl devices`:

```sh
phonebridge --bluetooth AA:BB:CC:DD:EE:FF
```

The RFCOMM channel is found through SDP (service UUID
`aabe3790-b3b9-400f-8a50-e9b0f38ac6b0`, falling back to Serial Port); force one
with `--bt-channel N`.

### Options

```
--token CODE       pairing code shown in the phone app: authenticates the phone and
                   encrypts audio and video (also $PHONEBRIDGE_TOKEN or
                   ~/.config/phonebridge/token)
--port PORT        control port configured in the phone app (default 8125)
--wifi HOST        connect over Wi-Fi to the phone's address instead of USB
--udp PORT         Wi-Fi: receive media on this UDP port (default: inline TCP)
--bluetooth ADDR   connect over Bluetooth to the paired phone
--bt-channel N     Bluetooth: RFCOMM channel (default: looked up with SDP)
--name NAME        name of the virtual microphone (default "Phonebridge"; no
                   " \ { } [ ] = or control characters)
--stdout           write raw s16le / 48 kHz / mono PCM to stdout instead
--no-adb           USB: don't run `adb forward` (port already forwarded)
--once             exit on the first disconnect (of either stream) instead of retrying
--camera           also stream the phone's camera to a v4l2loopback webcam
--no-mic           camera only: no audio connection, no virtual microphone
--video-port PORT  video port (default: control port + 1, i.e. 8126)
--video-device DEV v4l2loopback device (default: the one labelled "Phonebridge",
                   else the first v4l2loopback device)
--video-size WxH   fixed webcam size (default 1280x720)
```

Quick test, playing the phone straight to your speakers:

```sh
phonebridge --stdout --once | pw-play --raw --rate 48000 --channels 1 --format s16 -
```

### Webcam

Turn on **Share camera** in the phone app (USB or Wi-Fi; Bluetooth is too slow
for video), pick the camera, resolution and orientation, press **Start**, then:

```sh
phonebridge --camera                     # mic + webcam over USB
phonebridge --wifi 192.168.1.23 --camera # over Wi-Fi
phonebridge --camera --no-mic            # webcam only
```

In the background, `phonebridge camera toggle` starts or stops the webcam on its
own, next to the microphone ([background control](#background-control)).

The phone opens its camera only while the PC is connected. The video is decoded
by `ffmpeg` (bundled with the Nix package) and written to a v4l2loopback device,
which apps see as a normal webcam. Load the module once (on NixOS use the
[module](#nixos) instead):

```sh
sudo modprobe v4l2loopback video_nr=10 card_label=Phonebridge exclusive_caps=1
```

To make it permanent, put those options in `/etc/modprobe.d/` and
`v4l2loopback` in `/etc/modules-load.d/`.

The webcam has a **fixed format** for as long as `phonebridge --camera` runs:
1280x720 YUYV at 30 fps by default (`--video-size WxH` to change it). The
phone's picture is rotated upright and fitted in: same aspect ratio is scaled,
a portrait picture is centred over a blurred copy of itself. While the phone
isn't streaming (not started yet, camera sharing off, unplugged) the webcam
shows a **"Phonebridge — waiting for the phone camera" placeholder** at 5 fps,
and when the phone reconnects (switching camera, resolution or orientation)
the last picture stays for up to 3 s. So the device is always a valid camera
(`exclusive_caps=1` hides a loopback device that has no writer), and an app
that is using it never sees it vanish or change size: its video just resumes.

#### Virtual background

Turn on **Virtual background** in the phone app's camera settings and choose a
picture. The phone sends the picture to the PC with the video, and the PC cuts
you out of each frame and puts you in front of it. Segmentation uses
[MediaPipe Selfie Segmentation](models/README.md), run on the CPU by
[tract](https://github.com/sonos/tract), which is built into the binary (about
4 ms per frame, on its own thread so it never holds the video back). A portrait
picture in a landscape webcam fills the side bars with the picture too.
Turning it on or off, or choosing another picture, reconnects the stream, so
the last frame stays for a moment. If the phone app is older than this
feature, the client streams without a background.

#### Google Meet, Microsoft Teams and other browser apps

1. Start the webcam (`phonebridge camera start`, or the bar widget) **before**
   joining; the phone can be started later.
2. Pick **Phonebridge**: Meet → *Settings → Video → Camera*; Teams → *Settings →
   Devices → Camera* (or the camera menu next to the video button before
   joining).
3. If it isn't listed, reload the page; if it still isn't, restart the browser
   (Chrome reads the camera list once, and a device that appeared while it was
   running may need a restart).

Tested with Chromium and Firefox (`getUserMedia`: 1280x720 at 30 fps, frames keep
coming across phone reconnects and orientation changes). Firefox-based browsers
(Zen, LibreWolf) read the device directly by default; if
`media.webrtc.camera.allow-pipewire` is turned on in `about:config`, the camera
goes through the PipeWire camera portal instead.

### Background control

`phonebridge` also manages detached instances of itself, with desktop
notifications. The microphone and the webcam run as **separate instances**, so
each can be switched on and off on its own:

```
phonebridge toggle            start the microphone in the background, or stop it
phonebridge start [OPTIONS]   start in the background (OPTIONS as above)
phonebridge stop
phonebridge mute              mute / unmute (the input stays available and sends
                        silence; also `kill -USR1 <pid>`)
phonebridge status            print connected | muted | connecting | off
phonebridge logfile           print the log file (also `camera logfile`)

phonebridge camera toggle     start the webcam alone in the background, or stop it
phonebridge camera start [OPTIONS]
                              the same with options (runs `--camera --no-mic`)
phonebridge camera stop
phonebridge camera status     print streaming | connecting | off
phonebridge camera device     print the webcam device, e.g. /dev/video10
```

The camera instance only forwards the video port over adb and the microphone
instance only the control port, so they don't step on each other. Repeated
failed attempts (phone not plugged in, app not started) are logged once, not
on every retry. Don't also
pass `--camera` to the microphone instance (both would claim the same webcam).

**Default options.** `start` and `toggle` (and `camera start` / `camera toggle`)
without options read them from `$XDG_CONFIG_HOME/phonebridge/args`
(`~/.config/phonebridge/args`): whitespace-separated, `#` starts a comment. Use
it for the transport, so desktop and bar toggles connect over Wi-Fi:

```sh
mkdir -p ~/.config/phonebridge
echo "--wifi 192.168.1.23" > ~/.config/phonebridge/args
```

Keep to options that both instances accept (`--wifi`, `--port`, `--no-adb`);
`--bluetooth` works for the microphone only. Both instances also read the
[pairing code](#pairing-code) file, so keep the code there rather than in `args`.

The Nix package ships a **Phonebridge** desktop entry that runs `phonebridge toggle`.
Logs, pidfiles and status go to a private directory (mode 0700):
`$XDG_RUNTIME_DIR/phonebridge`, or `~/.local/state/phonebridge` (or
`$XDG_STATE_HOME/phonebridge`) when there is no runtime directory. The
microphone logs to `phonebridge.log`, the webcam to `camera.log`;
`phonebridge logfile` prints the path.

### Noctalia bar widgets

The package ships a [Noctalia](https://github.com/noctalia-dev/noctalia) plugin
under `share/noctalia-plugins/phonebridge` with two bar widgets.

Microphone (`wellington-vell/phonebridge:toggle`):

| Gesture | Action |
|---------|--------|
| Left click | start when off, otherwise mute / unmute |
| Right click | start / stop |
| Middle click | follow the log in a terminal |

The icon is highlighted when live, red when muted and crossed out when off.

Webcam (`wellington-vell/phonebridge:camera`):

| Gesture | Action |
|---------|--------|
| Left click | start / stop the webcam |
| Right click | while streaming, preview it (`mpv`, `ffplay` or `vlc`, whichever is installed) |
| Middle click | follow the camera log in a terminal |

The icon is highlighted while streaming, tinted while waiting for the phone and
crossed out when off. Both widgets run without options, so set the transport in
the [default options](#background-control) file if you don't use USB.

```nix
plugins.enabled = [ "wellington-vell/phonebridge" ];
plugins.source = [
  # declaring sources replaces the defaults — re-list official/community too
  { name = "phonebridge"; kind = "path"; location = "${phonebridge}/share/noctalia-plugins"; enabled = true; }
];
bar.default.end = [ "wellington-vell/phonebridge:toggle" "wellington-vell/phonebridge:camera" ];
```

### NixOS

```nix
# flake.nix
inputs.phonebridge.url = "github:wellington-vell/phonebridge";

# configuration
imports = [ inputs.phonebridge.nixosModules.default ];
programs.phonebridge = {
  enable = true;          # installs the client
  camera.enable = true;   # default: v4l2loopback webcam labelled "Phonebridge"
  camera.videoNr = 10;    # default: /dev/video10
};
```

`phonebridge camera start` (or `camera toggle`) runs the webcam in the
background; the desktop entry starts the microphone only.

## Android app

`app/` is a [Tauri 2](https://v2.tauri.app) Android app that plays the phone
side of the protocol, so it works with this client (and with anything else that
speaks it).

- **Transport**: USB, Wi-Fi or Bluetooth (RFCOMM, paired devices only)
- **Control port**: any port; USB/Bluetooth listen on localhost only, Wi-Fi on
  all interfaces (the screen shows the matching `phonebridge` command and addresses)
- **Audio source**: microphone, camcorder, voice recognition (no processing),
  voice call (echo / noise cancelling), unprocessed, voice performance
- **Volume** slider (0–400 %) with a level meter, and a big **mute** button;
  both apply live, muting sends silence so the PC input stays up
- **Camera**: live preview pinned at the top of the screen; share the back or
  front camera at 480p, 720p or 1080p (30 fps), landscape or portrait, over USB
  or Wi-Fi. Camera settings apply live without interrupting the microphone; if
  the camera permission is refused the mic still works
- **Virtual background**: choose a picture and the PC replaces what's behind
  you with it (see [Virtual background](#virtual-background))
- **Keep screen on while connected**, plus a foreground service so capture
  keeps running with the screen off

Capture (AAudio), Opus encoding (vendored libopus) and the protocol are in Rust
(`app/src-tauri/src`); the UI is plain HTML/JS (`app/ui`). A small Kotlin plugin
(`app/src-tauri/gen/android/app/src/main/java/.../PhonebridgePlugin.kt`) handles
permissions, keep-screen-on, the foreground service and the Bluetooth bridge,
which relays each RFCOMM client to the Rust server on localhost. The camera
(Camera2 → MediaCodec H.264) and its video server are in `CameraStreamer.kt`.

### Building

The flake in `app/` provides the Android SDK/NDK, JDK, Rust Android targets and
the Tauri CLI:

```sh
cd app
nix develop
cd src-tauri
cargo tauri android build --debug --apk --target aarch64   # installable debug APK
adb install gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
```

`cargo tauri android build --apk --target aarch64` makes a release APK
(`…/release/app-universal-release-unsigned.apk`); sign it with `apksigner`
before installing. Requires Android 9 or newer.

On a desktop, `cargo tauri dev` runs the same app with a 440 Hz test tone in
place of the microphone, which is handy for trying the client without a phone:
`phonebridge --wifi 127.0.0.1` (select Wi-Fi first).

## Protocol

Audio runs over a single stream: TCP to `127.0.0.1:<port>` after
`adb forward tcp:<port> tcp:<port>` (USB), TCP to `<phone>:<port>` (Wi-Fi) or
RFCOMM (Bluetooth).

Control messages (both directions), big-endian:

```
[type u8][length u32][payload: length bytes]
```

| Step | PC → phone | Payload | Phone reply payload |
|------|------------|---------|---------------------|
| Hello | `0x65` | `04 06 03 00 00 00` | `[status u8][media version u8]…` |
| Codec | `0x66` | `02 02` + UDP media port `u32` (0: media on this stream) | `[status u8]…` |
| Start | `0x67` | — | `[status u8]…` |

A status other than `0` means the phone rejected the request.

After **Start**, the phone sends media frames (endpoint version 4):

```
[0x04][0x00][length u16][seq u16][timestamp u32][flags u8][opus: length-7 bytes]
```

Audio is Opus, 48 kHz, mono, 20 ms frames. Missing sequence numbers are filled
with Opus packet-loss concealment. With a UDP media port, each datagram carries
one frame in the same layout.

Keepalive extension (the app in `app/` and this client): when the hello reply
ends with `wmka`, the client sends a `0x70` message with no payload every
second after start, and the phone drops a client that stops sending them for
4 s. That way a PC that disappeared without closing the connection (suspend,
Wi-Fi drop, Bluetooth out of range) doesn't keep the session alive. Neither
side sends or expects it otherwise.

Unknown / not implemented: the meaning of the `flags` byte, how the official
app's Wi-Fi and Bluetooth modes differ from the above (only tested against the
app in `app/`), and the older (< v4) media frame format.

### Video

A separate TCP stream on the video port (control port + 1, default 8126;
`adb forward` for USB, the phone's address for Wi-Fi). Phone side is the
server, one client at a time. Big-endian:

```
PC → phone   "PBV2" (or "PBV1")
phone → PC   the same magic, [status u8][codec u8][width u16][height u16][rotation u16][fps u8]
             on status != 0: [msg_len u16][UTF-8 message], then close
             PBV2, status 0: [bg_len u32][bg_len bytes of JPEG]
phone → PC   repeated: [len u32][pts_us u64][flags u8][len bytes of H.264 Annex-B]
```

- PBV2 adds the virtual background picture: a JPEG of at most 1920x1080
  (`bg_len` 0 = off; the PC refuses more than 4 MiB). Phone apps from before
  PBV2 close the connection on `"PBV2"`, and the PC then asks again with
  `"PBV1"`.

- `codec` 1 = H.264 Annex-B; `flags` bit 0 = keyframe, bit 1 = codec config.
- Each client first gets the SPS/PPS (codec config), then a keyframe; keyframes
  repeat every 2 s with SPS/PPS prepended.
- `rotation`: degrees clockwise to rotate the decoded image upright (from the
  sensor orientation and the orientation chosen in the app; not mirrored).
- `status` 2 = camera unavailable. When camera sharing is off, nothing listens.

## Disclaimer

Not affiliated with Wolicheng Tech. WO Mic is their product; this project only
provides an interoperable Linux client.

## License

[MIT](LICENSE)
