"""Generate the built-in virtual backgrounds (1920x1080 JPEG) and their
thumbnails. Procedural, so there are no licensing questions; soft and
defocused on purpose, which reads naturally behind a cut-out person.

    nix shell --impure --expr 'with import <nixpkgs> {}; python3.withPackages (p: [p.numpy p.pillow p.scipy])' \
      -c python3 app/backgrounds/generate.py

Writes app/src-tauri/gen/android/app/src/main/res/raw/bg_<name>.jpg (sent to
the PC) and app/ui/backgrounds/<name>.jpg (thumbnails for the picker).
"""

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw
from scipy.ndimage import gaussian_filter

W, H = 1920, 1080
ROOT = Path(__file__).resolve().parents[1]
RAW = ROOT / "src-tauri/gen/android/app/src/main/res/raw"
THUMBS = ROOT / "ui/backgrounds"
rng = np.random.default_rng(7)

yy, xx = np.mgrid[0:H, 0:W].astype(np.float32)
u, v = xx / W, yy / H


def hex_rgb(h):
    return np.array([int(h[i : i + 2], 16) for i in (1, 3, 5)], np.float32) / 255


def mix(a, b, t):
    return a * (1 - t[..., None]) + b * t[..., None]


def blur(img, r):
    """Gaussian blur (per channel for RGB), in float precision."""
    sigma = (r, r, 0) if img.ndim == 3 else r
    return gaussian_filter(img, sigma, mode="nearest")


def finish(img, name):
    # Fine grain: no banding in gradients after JPEG and H.264.
    img = img + rng.normal(0, 1.2 / 255, img.shape).astype(np.float32)
    out = Image.fromarray((np.clip(img, 0, 1) * 255 + 0.5).astype(np.uint8))
    RAW.mkdir(parents=True, exist_ok=True)
    THUMBS.mkdir(parents=True, exist_ok=True)
    out.save(RAW / f"bg_{name}.jpg", quality=88, optimize=True, progressive=True)
    out.resize((320, 180), Image.LANCZOS).save(THUMBS / f"{name}.jpg", quality=82, optimize=True)


def studio():
    """Graphite backdrop, a soft light behind the head, vignette."""
    d = np.hypot((u - 0.5) * 1.5, (v - 0.4) * 1.1)
    light = np.exp(-(d**2) / 0.12)
    img = mix(hex_rgb("#1b1e23"), hex_rgb("#5d636c"), light)
    # A touch of cool falloff towards the floor.
    img = mix(img, hex_rgb("#15181c"), np.clip((v - 0.65) / 0.35, 0, 1) ** 1.5 * 0.6)
    vignette = 1 - 0.35 * np.clip(np.hypot(u - 0.5, v - 0.5) / 0.75, 0, 1) ** 2
    return img * vignette[..., None]


def office():
    """A modern office out of focus: window light on the right, shelves,
    a plant and pendant lights, then bokeh."""
    img = mix(hex_rgb("#cfc8bd"), hex_rgb("#8f877c"), v**1.3)  # warm wall
    canvas = Image.fromarray((img * 255).astype(np.uint8))
    d = ImageDraw.Draw(canvas)
    # Window wall on the right with mullions.
    d.rectangle([1260, 90, 1920, 860], fill=(214, 228, 238))
    for x in (1260, 1480, 1700, 1920):
        d.rectangle([x - 14, 90, x + 14, 860], fill=(70, 74, 80))
    d.rectangle([1250, 470, 1920, 488], fill=(70, 74, 80))
    # Shelving on the left with books / objects.
    for y in (300, 520, 740):
        d.rectangle([60, y, 640, y + 16], fill=(92, 72, 56))
    colors = [(60, 74, 92), (140, 112, 82), (196, 188, 172), (88, 98, 84), (150, 70, 52)]
    for row, y in enumerate((300, 520, 740)):
        x = 90 + row * 30
        while x < 600:
            w = int(rng.integers(26, 60))
            h = int(rng.integers(90, 170))
            d.rectangle([x, y - h, x + w, y], fill=colors[int(rng.integers(len(colors)))])
            x += w + int(rng.integers(4, 40))
    # Desk and floor.
    d.rectangle([0, 860, 1920, 1080], fill=(74, 66, 58))
    d.rectangle([380, 800, 1500, 840], fill=(120, 96, 74))
    # Tall plant by the window, muted.
    for _ in range(90):
        cx, cy = 1150 + rng.normal(0, 55), 560 + rng.normal(0, 130)
        r = rng.uniform(14, 34)
        g = int(rng.integers(86, 112))
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(70, g, 76))
    d.rectangle([1112, 770, 1188, 860], fill=(196, 190, 180))
    img = np.asarray(canvas, np.float32) / 255
    img = blur(img, 38)
    # Window glow spilling into the room.
    glow = np.exp(-(((u - 0.86) / 0.25) ** 2 + ((v - 0.4) / 0.45) ** 2))
    img = img + glow[..., None] * hex_rgb("#fff6e8") * 0.18
    # Bokeh: soft discs from pendant lights and reflections.
    bokeh = np.zeros((H, W), np.float32)
    for _ in range(11):
        cx, cy = rng.uniform(150, W - 100), rng.uniform(40, 300)
        r = rng.uniform(18, 42)
        dist = np.hypot(xx - cx, yy - cy)
        bokeh += np.clip((r - dist) / 4, 0, 1) * rng.uniform(0.05, 0.12)
    bokeh = blur(bokeh, 3)
    img = img + bokeh[..., None] * hex_rgb("#ffd9a0")
    return np.clip(img, 0, 1) * 0.97


def daylight():
    """A warm, light wall with soft window-blind shadows."""
    img = mix(hex_rgb("#efe8dc"), hex_rgb("#cfc5b6"), np.clip(v * 0.8 + u * 0.3, 0, 1))
    # Slats across a window-shaped patch of sunlight, slanted.
    a = np.deg2rad(-24)
    s = (xx * np.cos(a) - yy * np.sin(a)) / W
    t = (xx * np.sin(a) + yy * np.cos(a)) / H
    patch = (
        np.clip((s - 0.18) / 0.05, 0, 1)
        * np.clip((0.78 - s) / 0.05, 0, 1)
        * np.clip((t - 0.05) / 0.05, 0, 1)
        * np.clip((0.95 - t) / 0.08, 0, 1)
    )
    slats = 0.5 + 0.5 * np.sin(t * np.pi * 2 * 11)
    slats = np.clip((slats - 0.3) / 0.4, 0, 1)
    sun = patch * slats
    sun = blur(sun.astype(np.float32), 20)
    img = img * (1 - 0.06) + sun[..., None] * hex_rgb("#fff1d6") * 0.13
    vignette = 1 - 0.18 * np.clip(np.hypot(u - 0.5, v - 0.45) / 0.8, 0, 1) ** 2
    return img * vignette[..., None]


for name, make in [("studio", studio), ("office", office), ("daylight", daylight)]:
    finish(make(), name)
    print(name)
