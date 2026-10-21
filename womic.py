#!/usr/bin/env python3
"""WO Mic Linux client (USB mode).

Protocol reverse engineered from WOMicClient.exe 6.3:

  Transport (USB): `adb forward tcp:P tcp:P`, then TCP to 127.0.0.1:P.
  Control messages (both directions): [type u8][len u32 BE][payload len bytes]
    0x65 hello       -> payload 04 06 03 00 00 00
                     <- payload [status u8][media version u8][...]
    0x66 set codec   -> payload 02 02 [media UDP port u32 BE]  (0 for USB)
                     <- payload [status u8][...]
    0x67 start       -> no payload
                     <- payload [status u8][...]
  Media frames (V4, same TCP socket after start):
    [0x04][0x00][len u16 BE][seq u16 BE][timestamp u32 BE][flags u8][opus: len-7 bytes]
  Codec: Opus, 48 kHz, mono, 20 ms (960 samples) frames.
"""
import argparse
import ctypes
import ctypes.util
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time

RATE = 48000
CHANNELS = 1
MAX_FRAME = 5760  # 120 ms @ 48 kHz, largest Opus frame


def log(*a):
    print("[womic]", *a, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------- opus
def load_opus():
    candidates = [os.environ.get("LIBOPUS"), ctypes.util.find_library("opus"), "libopus.so.0", "libopus.so"]
    for c in candidates:
        if not c:
            continue
        try:
            lib = ctypes.CDLL(c)
            break
        except OSError:
            continue
    else:
        sys.exit("libopus not found (set LIBOPUS=/path/to/libopus.so.0 or run inside nix-shell)")
    lib.opus_decoder_create.restype = ctypes.c_void_p
    lib.opus_decoder_create.argtypes = [ctypes.c_int32, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
    lib.opus_decode.restype = ctypes.c_int
    lib.opus_decode.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int32,
                                ctypes.POINTER(ctypes.c_int16), ctypes.c_int, ctypes.c_int]
    lib.opus_strerror.restype = ctypes.c_char_p
    return lib


class OpusDecoder:
    def __init__(self, lib):
        self.lib = lib
        err = ctypes.c_int()
        self.dec = lib.opus_decoder_create(RATE, CHANNELS, ctypes.byref(err))
        if err.value != 0:
            raise RuntimeError(lib.opus_strerror(err.value).decode())
        self.pcm = (ctypes.c_int16 * (MAX_FRAME * CHANNELS))()

    def decode(self, data):
        # data=None => packet loss concealment
        n = self.lib.opus_decode(self.dec, data, len(data) if data else 0, self.pcm, MAX_FRAME, 0)
        if n < 0:
            raise RuntimeError(self.lib.opus_strerror(n).decode())
        return ctypes.string_at(self.pcm, n * CHANNELS * 2)


# ---------------------------------------------------------------------- transport
def adb(*args, check=True):
    return subprocess.run(["adb", *args], capture_output=True, text=True, check=check)


def recv_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed by phone")
        buf += chunk
    return bytes(buf)


def send_msg(sock, mtype, payload=b""):
    sock.sendall(struct.pack(">BI", mtype, len(payload)) + payload)


def recv_reply(sock, expected):
    mtype, length = struct.unpack(">BI", recv_exact(sock, 5))
    payload = recv_exact(sock, length) if length else b""
    if mtype != expected:
        raise ConnectionError(f"unexpected reply type 0x{mtype:02x} (wanted 0x{expected:02x})")
    status = payload[0] if payload else 0
    if status != 0:
        raise ConnectionError(f"phone rejected 0x{expected:02x} with status {status}")
    return payload


def handshake(sock):
    send_msg(sock, 0x65, bytes([4, 6, 3, 0, 0, 0]))
    reply = recv_reply(sock, 0x65)
    version = min(reply[1] if len(reply) > 1 else 4, 4)
    log(f"hello ok, media endpoint version {version}")
    send_msg(sock, 0x66, bytes([2, 2]) + struct.pack(">I", 0))
    recv_reply(sock, 0x66)
    log("codec parameters set")
    send_msg(sock, 0x67)
    recv_reply(sock, 0x67)
    log("streaming started")
    return version


def frames(sock):
    """Yield (seq, timestamp, opus_payload) from V4 media frames."""
    while True:
        hdr = recv_exact(sock, 11)
        mtype, zero, length, seq, ts, _flags = struct.unpack(">BBHHIB", hdr)
        body = recv_exact(sock, length - 7) if length > 7 else b""
        if mtype != 4 or zero != 0:
            log(f"skipping non-media frame type=0x{mtype:02x} len={length}")
            continue
        yield seq, ts, body


# ------------------------------------------------------------------------- output
class VirtualMic:
    """PipeWire virtual microphone: pw-loopback (sink -> source) fed by pw-cat."""

    SINK = "womic_sink"
    SOURCE = "womic"

    def __init__(self, name):
        self.loop = subprocess.Popen([
            "pw-loopback",
            "--capture-props", f"media.class=Audio/Sink node.name={self.SINK} "
                               f"node.description=\"{name} (input)\" audio.channels=1 audio.position=[MONO]",
            "--playback-props", f"media.class=Audio/Source node.name={self.SOURCE} "
                                f"node.description=\"{name}\" audio.channels=1 audio.position=[MONO]",
        ])
        time.sleep(0.5)
        self.cat = subprocess.Popen([
            "pw-cat", "--playback", "--raw", "--target", self.SINK,
            "--rate", str(RATE), "--channels", str(CHANNELS), "--format", "s16",
            "--latency", "40ms", "-",
        ], stdin=subprocess.PIPE)

    def write(self, pcm):
        self.cat.stdin.write(pcm)
        self.cat.stdin.flush()

    def close(self):
        for p in (self.cat, self.loop):
            try:
                p.terminate()
                p.wait(timeout=2)
            except Exception:
                pass


class StdoutSink:
    def write(self, pcm):
        sys.stdout.buffer.write(pcm)

    def close(self):
        sys.stdout.buffer.flush()


# --------------------------------------------------------------------------- mute
class Mute:
    """SIGUSR1 toggles mute. While muted, silence is sent so the source stays alive.

    State is mirrored to $XDG_RUNTIME_DIR/womic/muted for status queries.
    """

    def __init__(self):
        self.on = False
        self.flag = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "womic", "muted")
        self._sync()
        signal.signal(signal.SIGUSR1, self.toggle)

    def toggle(self, *_):
        self.on = not self.on
        self._sync()
        log("muted" if self.on else "unmuted")

    def _sync(self):
        try:
            if self.on:
                os.makedirs(os.path.dirname(self.flag), exist_ok=True)
                open(self.flag, "w").close()
            elif os.path.exists(self.flag):
                os.remove(self.flag)
        except OSError:
            pass

    def apply(self, pcm):
        return bytes(len(pcm)) if self.on else pcm


# --------------------------------------------------------------------------- main
def run(args):
    opus = OpusDecoder(load_opus())
    if not args.no_adb:
        if not shutil.which("adb"):
            sys.exit("adb not found (nix-shell -p android-tools)")
        devs = [l for l in adb("devices").stdout.splitlines()[1:] if l.strip().endswith("device")]
        if not devs:
            sys.exit("no adb device; enable USB debugging and accept the prompt on the phone")
        adb("forward", f"tcp:{args.port}", f"tcp:{args.port}")
        log(f"adb forward tcp:{args.port} ready")

    out = StdoutSink() if args.stdout else VirtualMic(args.name)
    mute = Mute()
    try:
        while True:
            try:
                with socket.create_connection(("127.0.0.1", args.port), timeout=5) as sock:
                    handshake(sock)
                    sock.settimeout(10)
                    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    last_seq = None
                    for seq, _ts, payload in frames(sock):
                        if last_seq is not None:
                            lost = (seq - last_seq - 1) & 0xFFFF
                            for _ in range(min(lost, 5)):
                                out.write(mute.apply(opus.decode(None)))
                        last_seq = seq
                        out.write(mute.apply(opus.decode(payload)))
            except (ConnectionError, socket.timeout, OSError) as e:
                log(f"disconnected: {e}")
                if args.once:
                    return 1
                log("retrying in 2s (is WO Mic started on the phone with transport=USB?)")
                time.sleep(2)
    finally:
        if mute.on:
            mute.toggle()
        out.close()
        if not args.no_adb:
            adb("forward", "--remove", f"tcp:{args.port}", check=False)


def main():
    p = argparse.ArgumentParser(description="WO Mic client for Linux (USB via adb)")
    p.add_argument("--port", type=int, default=8125, help="control port (default 8125)")
    p.add_argument("--name", default="WO Mic", help="virtual microphone name")
    p.add_argument("--stdout", action="store_true", help="write raw s16le 48k mono PCM to stdout instead")
    p.add_argument("--no-adb", action="store_true", help="skip adb forward (port already forwarded)")
    p.add_argument("--once", action="store_true", help="exit on disconnect instead of retrying")
    args = p.parse_args()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        sys.exit(run(args))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
