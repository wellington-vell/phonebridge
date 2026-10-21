# wo-mic-linux

Use your Android phone as a microphone on Linux with the
[WO Mic](https://wolicheng.com/womic) Android app. This is an independent client
that speaks the WO Mic protocol over USB (through `adb`) and exposes the phone
as a PipeWire virtual microphone named **"WO Mic"**.

The protocol was reverse engineered from the official Windows client
(`WOMicClient.exe` 6.3). No Windows code is included or needed.

## Requirements

- Linux with PipeWire (`pw-loopback`, `pw-cat`)
- `adb` (Android platform tools)
- `libopus`
- Python 3 (standard library only)
- WO Mic app on the phone, USB debugging enabled

With Nix all of these are provided for you.

## Usage

1. On the phone: enable **Developer options → USB debugging**, plug it in and
   accept the authorization prompt.
2. In the WO Mic app: **Settings → Transport → USB**, then press **Start**.
3. On the PC:

   ```sh
   nix run github:wellington-vell/wo-mic-linux
   # or, without Nix
   python3 womic.py
   ```

4. Select **WO Mic** as the input device in your apps (or `wpctl set-default`).

Stop with `Ctrl+C`. The client reconnects automatically if the phone app stops
or the cable is unplugged.

### Options

```
--port PORT   control port configured in the phone app (default 8125)
--name NAME   name of the virtual microphone (default "WO Mic")
--stdout      write raw s16le / 48 kHz / mono PCM to stdout instead
--no-adb      don't run `adb forward` (port already forwarded)
--once        exit on disconnect instead of retrying
```

Quick test, playing the phone straight to your speakers:

```sh
python3 womic.py --stdout --once | pw-play --raw --rate 48000 --channels 1 --format s16 -
```

### Launcher toggle

The Nix package also ships `womic-toggle` and a **WO Mic** desktop entry. Opening
it starts the client in the background (with notifications); opening it again
stops it. Logs go to `$XDG_RUNTIME_DIR/womic/womic.log`.
`womic-toggle status` prints `connected`, `connecting` or `off`.

### Noctalia bar toggle

The package ships a [Noctalia](https://github.com/noctalia-dev/noctalia) plugin
under `share/noctalia-plugins/womic`. Click toggles the mic, right-click tails
the log; the icon is highlighted when connected and crossed out when off.

```nix
plugins.enabled = [ "wellington-vell/womic" ];
plugins.source = [
  # declaring sources replaces the defaults — re-list official/community too
  { name = "womic"; kind = "path"; location = "${wo-mic-linux}/share/noctalia-plugins"; enabled = true; }
];
bar.default.end = [ "wellington-vell/womic:toggle" ];
```

### NixOS

```nix
# flake.nix
inputs.wo-mic-linux.url = "github:wellington-vell/wo-mic-linux";

# configuration
environment.systemPackages = [
  inputs.wo-mic-linux.packages.${pkgs.stdenv.hostPlatform.system}.default
];
```

## Protocol

Everything runs over a single TCP connection to `127.0.0.1:<port>` after
`adb forward tcp:<port> tcp:<port>`.

Control messages (both directions), big-endian:

```
[type u8][length u32][payload: length bytes]
```

| Step | PC → phone | Payload | Phone reply payload |
|------|------------|---------|---------------------|
| Hello | `0x65` | `04 06 03 00 00 00` | `[status u8][media version u8]…` |
| Codec | `0x66` | `02 02` + UDP media port `u32` (0 for USB) | `[status u8]…` |
| Start | `0x67` | — | `[status u8]…` |

A status other than `0` means the phone rejected the request.

After **Start**, the phone sends media frames (endpoint version 4):

```
[0x04][0x00][length u16][seq u16][timestamp u32][flags u8][opus: length-7 bytes]
```

Audio is Opus, 48 kHz, mono, 20 ms frames. Missing sequence numbers are filled
with Opus packet-loss concealment.

Unknown / not implemented: the meaning of the `flags` byte, the Wi-Fi (UDP) and
Bluetooth transports, and the older (< v4) media frame format.

## Disclaimer

Not affiliated with Wolicheng Tech. WO Mic is their product; this project only
provides an interoperable Linux client.

## License

[MIT](LICENSE)
