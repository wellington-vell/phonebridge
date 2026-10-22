# WeMic

Use your Android phone as a microphone on Linux. This is an independent client
that speaks the [WO Mic](https://wolicheng.com/womic) protocol over **USB**
(through `adb`), **Wi-Fi** or **Bluetooth** and exposes the phone as a PipeWire
virtual microphone named **"WeMic"**.

On the phone, use the [WeMic Android app](#android-app) in `app/` (Tauri), which
adds a volume slider, a mute button, audio source selection and keep-screen-on,
or the official WO Mic app.

The protocol was reverse engineered from the official Windows client
(`WOMicClient.exe` 6.3). No Windows code is included or needed.

## Requirements

- Linux with PipeWire (`pw-loopback`, `pw-cat`)
- `adb` (Android platform tools) for USB
- BlueZ for Bluetooth
- `libopus`
- Rust toolchain (to build without Nix)
- On the phone: the WeMic app in `app/`, or the official WO Mic app

With Nix all of these are provided for you.

## Usage

1. On the phone: enable **Developer options → USB debugging**, plug it in and
   accept the authorization prompt.
2. In the phone app select **USB** (official WO Mic: **Settings → Transport →
   USB**), then press **Start**.
3. On the PC:

   ```sh
   nix run github:wellington-vell/wo-mic-linux
   # or, without Nix (needs libopus development files)
   cargo run --release
   ```

4. Select **WeMic** as the input device in your apps (or `wpctl set-default`).

Stop with `Ctrl+C`. The client reconnects automatically if the phone app stops
or the cable is unplugged.

### Wi-Fi

Phone and PC on the same network; select **Wi-Fi** in the phone app and use the
address it shows:

```sh
wemic --wifi 192.168.1.23
```

Media comes over the same TCP connection by default. `--udp PORT` asks the phone
to send it as UDP datagrams to that local port instead (open it in your firewall).

### Bluetooth

Pair the phone with the PC once (`bluetoothctl`, or your desktop's settings),
select **Bluetooth** in the phone app, then use the phone's address from
`bluetoothctl devices`:

```sh
wemic --bluetooth AA:BB:CC:DD:EE:FF
```

The RFCOMM channel is found through SDP (service UUID
`aabe3790-b3b9-400f-8a50-e9b0f38ac6b0`, falling back to Serial Port); force one
with `--bt-channel N`.

### Options

```
--port PORT        control port configured in the phone app (default 8125)
--wifi HOST        connect over Wi-Fi to the phone's address instead of USB
--udp PORT         Wi-Fi: receive media on this UDP port (default: inline TCP)
--bluetooth ADDR   connect over Bluetooth to the paired phone
--bt-channel N     Bluetooth: RFCOMM channel (default: looked up with SDP)
--name NAME        name of the virtual microphone (default "WeMic")
--stdout           write raw s16le / 48 kHz / mono PCM to stdout instead
--no-adb           USB: don't run `adb forward` (port already forwarded)
--once             exit on disconnect instead of retrying
```

Quick test, playing the phone straight to your speakers:

```sh
wemic --stdout --once | pw-play --raw --rate 48000 --channels 1 --format s16 -
```

### Background control

`wemic` also manages a detached instance of itself, with desktop notifications:

```
wemic toggle            start in the background, or stop it if running
wemic start [OPTIONS]   start in the background (OPTIONS as above)
wemic stop
wemic mute              mute / unmute (the input stays available and sends
                        silence; also `kill -USR1 <pid>`)
wemic status            print connected | muted | connecting | off
```

The Nix package ships a **WeMic** desktop entry that runs `wemic toggle`.
Logs go to `$XDG_RUNTIME_DIR/wemic/wemic.log`.

### Noctalia bar toggle

The package ships a [Noctalia](https://github.com/noctalia-dev/noctalia) plugin
under `share/noctalia-plugins/wemic`:

| Gesture | Action |
|---------|--------|
| Left click | start when off, otherwise mute / unmute |
| Right click | start / stop |
| Middle click | follow the log in a terminal |

The icon is highlighted when live, red when muted and crossed out when off.

```nix
plugins.enabled = [ "wellington-vell/wemic" ];
plugins.source = [
  # declaring sources replaces the defaults — re-list official/community too
  { name = "wemic"; kind = "path"; location = "${wemic}/share/noctalia-plugins"; enabled = true; }
];
bar.default.end = [ "wellington-vell/wemic:toggle" ];
```

### NixOS

```nix
# flake.nix
inputs.wemic.url = "github:wellington-vell/wo-mic-linux";

# configuration
environment.systemPackages = [
  inputs.wemic.packages.${pkgs.stdenv.hostPlatform.system}.default
];
```

## Android app

`app/` is a [Tauri 2](https://v2.tauri.app) Android app that plays the phone
side of the protocol, so it works with this client (and with anything else that
speaks it).

- **Transport**: USB, Wi-Fi or Bluetooth (RFCOMM, paired devices only)
- **Control port**: any port; USB/Bluetooth listen on localhost only, Wi-Fi on
  all interfaces (the screen shows the matching `wemic` command and addresses)
- **Audio source**: microphone, camcorder, voice recognition (no processing),
  voice call (echo / noise cancelling), unprocessed, voice performance
- **Volume** slider (0–400 %) with a level meter, and a big **mute** button;
  both apply live, muting sends silence so the PC input stays up
- **Keep screen on while connected**, plus a foreground service so capture
  keeps running with the screen off

Capture (AAudio), Opus encoding (vendored libopus) and the protocol are in Rust
(`app/src-tauri/src`); the UI is plain HTML/JS (`app/ui`). A small Kotlin plugin
(`app/src-tauri/gen/android/app/src/main/java/.../WemicPlugin.kt`) handles
permissions, keep-screen-on, the foreground service and the Bluetooth bridge,
which relays each RFCOMM client to the Rust server on localhost.

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
`wemic --wifi 127.0.0.1` (select Wi-Fi first).

## Protocol

Everything runs over a single stream: TCP to `127.0.0.1:<port>` after
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

## Disclaimer

Not affiliated with Wolicheng Tech. WO Mic is their product; this project only
provides an interoperable Linux client.

## License

[MIT](LICENSE)
