"""Generate the animated "Fireplace" virtual background: a blocky pixel-art
cabin at night (16x16 block textures, Minecraft style) with a crackling
fire, as a looping 320x180 GIF. The PC scales it up with nearest-neighbour,
so the pixels stay crisp (4x for 720p, 6x for 1080p). Procedural, so there
are no licensing questions.

    nix shell --impure --expr 'with import <nixpkgs> {}; python3.withPackages (p: [p.numpy p.pillow p.scipy])' \
      -c python3 app/backgrounds/fireplace.py

Writes app/src-tauri/gen/android/app/src/main/res/raw/bg_fireplace.gif (sent
to the PC) and app/ui/backgrounds/fireplace.gif (the picker's tile, same
file).
"""

from pathlib import Path

import numpy as np
from PIL import Image
from scipy.ndimage import gaussian_filter

W, H = 320, 180
B = 16  # block size
COLS, ROWS = W // B, 12  # drawn 192 tall, the bottom 12 rows cropped
FRAMES, FPS = 20, 10  # 2 s loop; the PC plays GIFs at 10 fps
ROOT = Path(__file__).resolve().parents[1]
RAW = ROOT / "src-tauri/gen/android/app/src/main/res/raw"
THUMBS = ROOT / "ui/backgrounds"
rng = np.random.default_rng(11)


def rgb(*c):
    return np.array(c, np.float32) / 255


def jitter(tile, amount):
    """Per-pixel brightness noise, the grainy look of block textures."""
    n = rng.integers(-amount, amount + 1, tile.shape[:2]).astype(np.float32) / 255
    return np.clip(tile + n[..., None], 0, 1)


# ----------------------------------------------------------------- textures
def planks(base, seam, horizontal=True):
    t = np.tile(base, (B, B, 1))
    for y in range(0, B, 4):
        t[y + 3] = seam  # board edges
        x = int(rng.integers(0, B))
        t[y : y + 3, x] = seam  # board ends, staggered
        t[y : y + 3] *= rng.uniform(0.92, 1.05)
    t = jitter(t, 8)
    return t if horizontal else t.transpose(1, 0, 2)


def log_side(bark, groove):
    t = np.tile(bark, (B, B, 1))
    for x in range(B):
        if rng.random() < 0.35:
            y0 = int(rng.integers(0, B))
            t[y0 : y0 + int(rng.integers(4, 12)), x] = groove
    return jitter(t, 7)


def cobble():
    """Stones from nearest-seed cells, mortar where two cells meet."""
    seeds = rng.uniform(0, B, (9, 2))
    tones = [rgb(128, 128, 128), rgb(110, 110, 110), rgb(96, 96, 98), rgb(138, 136, 134)]
    yy, xx = np.mgrid[0:B, 0:B]
    best = np.full((B, B), 1e9)
    second = np.full((B, B), 1e9)
    owner = np.zeros((B, B), int)
    for i, (sx, sy) in enumerate(seeds):
        # Wrap around, so blocks tile without seams.
        dx = np.minimum(abs(xx - sx), B - abs(xx - sx))
        dy = np.minimum(abs(yy - sy), B - abs(yy - sy))
        d = np.hypot(dx, dy)
        closer = d < best
        second = np.where(closer, best, np.minimum(second, d))
        owner = np.where(closer, i, owner)
        best = np.where(closer, d, best)
    t = np.array([tones[o % len(tones)] for o in owner.ravel()]).reshape(B, B, 3)
    t[(second - best) < 1.1] = rgb(70, 70, 72)
    return jitter(t, 10)


def netherrack():
    t = np.tile(rgb(112, 46, 46), (B, B, 1))
    t[rng.random((B, B)) < 0.3] = rgb(82, 30, 32)
    return jitter(t, 10)


def bookshelf(oak):
    t = oak.copy()
    colors = [(124, 36, 36), (46, 74, 120), (60, 104, 52), (150, 120, 60), (96, 60, 110), (170, 160, 140)]
    for top in (1, 9):
        x = 1
        t[top : top + 6, :] = rgb(54, 38, 22)  # shadow behind the books
        while x < B - 1:
            w = int(rng.integers(1, 3))
            h = int(rng.integers(4, 7))
            c = rgb(*colors[int(rng.integers(len(colors)))])
            t[top + 6 - h : top + 6, x : x + w] = c * rng.uniform(0.85, 1.05)
            x += w
    return jitter(t, 4)


oak = rgb(162, 130, 78), rgb(116, 90, 52)
spruce = rgb(112, 82, 50), rgb(82, 58, 34)
OAK_PLANKS = [planks(*oak) for _ in range(4)]
SPRUCE_PLANKS = [planks(*spruce) for _ in range(4)]
DARK_OAK = [planks(rgb(72, 48, 24), rgb(50, 32, 15)) for _ in range(3)]
OAK_LOG = [log_side(rgb(108, 86, 52), rgb(78, 60, 34)) for _ in range(3)]
COBBLE = [cobble() for _ in range(5)]
NETHERRACK = netherrack()
BOOKSHELF = [bookshelf(OAK_PLANKS[i]) for i in range(3)]


# --------------------------------------------------------------------- room
albedo = np.zeros((ROWS * B, W, 3), np.float32)
# Light the fire throws: 0 for things it cannot reach (the night outside).
lit = np.ones((ROWS * B, W), np.float32)


def put(tex, bx, by):
    albedo[by * B : (by + 1) * B, bx * B : (bx + 1) * B] = tex


def pick(texes):
    return texes[int(rng.integers(len(texes)))]


for by in range(ROWS):
    for bx in range(COLS):
        if by == 0:
            put(pick(DARK_OAK), bx, by)
        elif by >= 10:
            put(pick(OAK_PLANKS), bx, by)
        elif bx in (0, 9, COLS - 1):
            put(pick(OAK_LOG), bx, by)
        else:
            put(pick(SPRUCE_PLANKS), bx, by)

# Bookshelves on the left, a window above them.
for bx in (1, 2, 3, 4):
    for by in (8, 9):
        put(pick(BOOKSHELF), bx, by)
# A dark oak slab on top of them.
albedo[8 * B - 4 : 8 * B, 1 * B : 5 * B] = np.tile(DARK_OAK[0][:4], (1, 4, 1))

WIN = (2 * B, 3 * B, 5 * B, 6 * B)  # x0, y0, x1, y1
wx0, wy0, wx1, wy1 = WIN
sky_y = np.linspace(0, 1, wy1 - wy0)[:, None, None]
albedo[wy0:wy1, wx0:wx1] = rgb(18, 24, 58) * (1 - sky_y) + rgb(40, 52, 96) * sky_y
lit[wy0:wy1, wx0:wx1] = 0
# Moon: a blocky square, Minecraft style.
albedo[wy0 + 6 : wy0 + 14, wx0 + 30 : wx0 + 38] = rgb(236, 236, 214)
albedo[wy0 + 7 : wy0 + 9, wx0 + 32 : wx0 + 34] = rgb(200, 200, 182)
albedo[wy0 + 10 : wy0 + 12, wx0 + 35 : wx0 + 37] = rgb(206, 206, 188)
# Distant hills with pine tops along the bottom of the window.
hill = (wy1 - 9 + 3 * np.sin(np.arange(wx1 - wx0) / 5.0)).astype(int)
for i, top in enumerate(hill):
    albedo[top:wy1, wx0 + i] = rgb(16, 28, 30)
    if i % 7 == 3:
        albedo[top - 6 : top, wx0 + i] = rgb(14, 34, 28)
        albedo[top - 4 : top, wx0 + i - 1 : wx0 + i + 2] = rgb(14, 34, 28)
stars = [(int(rng.integers(wx0 + 2, wx1 - 2)), int(rng.integers(wy0 + 2, wy0 + 26))) for _ in range(14)]
stars = [(x, y) for x, y in stars if not (wx0 + 28 <= x <= wx0 + 40 and y <= wy0 + 16)]
star_phase = rng.integers(0, FRAMES, len(stars))
# Window frame (dark oak) and the cross of the panes.
FRAME = rgb(60, 40, 20)
albedo[wy0 - 2 : wy0, wx0 - 2 : wx1 + 2] = FRAME
albedo[wy1 : wy1 + 2, wx0 - 2 : wx1 + 2] = FRAME
albedo[wy0 - 2 : wy1 + 2, wx0 - 2 : wx0] = FRAME
albedo[wy0 - 2 : wy1 + 2, wx1 : wx1 + 2] = FRAME
albedo[wy0:wy1, (wx0 + wx1) // 2 - 1 : (wx0 + wx1) // 2 + 1] = FRAME
albedo[(wy0 + wy1) // 2 - 1 : (wy0 + wy1) // 2 + 1, wx0:wx1] = FRAME
lit[wy0:wy1, (wx0 + wx1) // 2 - 1 : (wx0 + wx1) // 2 + 1] = 1
lit[(wy0 + wy1) // 2 - 1 : (wy0 + wy1) // 2 + 1, wx0:wx1] = 1

# Fireplace on the right: cobblestone surround and chimney, a mantel, a
# 3-block opening with netherrack and logs.
for by in range(1, 10):
    for bx in range(14, 19):
        chimney = by <= 6 and bx in (15, 16, 17)
        if by >= 7 or chimney:
            put(pick(COBBLE), bx, by)
OPEN = (15 * B, 8 * B, 18 * B, 10 * B)  # x0, y0, x1, y1
ox0, oy0, ox1, oy1 = OPEN
albedo[oy0:oy1, ox0:ox1] *= 0.35  # soot-dark back wall
albedo[oy1 - 4 : oy1, ox0:ox1] = np.tile(NETHERRACK[:4], (1, 3, 1))
# Two logs on the grate.
for lx0, lx1, ly in ((ox0 + 6, ox0 + 26, oy1 - 8), (ox0 + 20, ox1 - 6, oy1 - 7)):
    albedo[ly : ly + 4, lx0:lx1] = rgb(92, 70, 40)
    albedo[ly + 1, lx0:lx1:3] = rgb(64, 48, 26)
    albedo[ly : ly + 4, lx0 : lx0 + 2] = rgb(170, 138, 86)
# Mantel: a dark oak slab sticking out over the surround.
albedo[7 * B - 4 : 7 * B + 2, 13 * B + 8 : 19 * B + 8] = rgb(74, 50, 26)
albedo[7 * B - 4, 13 * B + 8 : 19 * B + 8] = rgb(96, 68, 38)
albedo[7 * B + 2 : 7 * B + 4, 13 * B + 8 : 19 * B + 8] = rgb(40, 26, 12)
# Things on the mantel: a candle, a flower pot, a lantern.
CANDLE = (14 * B + 4, 7 * B - 10)
albedo[7 * B - 10 : 7 * B - 4, 14 * B + 3 : 14 * B + 6] = rgb(226, 214, 180)
albedo[7 * B - 11, 14 * B + 4] = rgb(40, 30, 20)
albedo[7 * B - 10 : 7 * B - 4, 16 * B + 4 : 16 * B + 12] = rgb(150, 76, 50)
albedo[7 * B - 10, 16 * B + 4 : 16 * B + 12] = rgb(176, 96, 64)
albedo[7 * B - 15 : 7 * B - 10, 16 * B + 6 : 16 * B + 10] = rgb(56, 116, 48)
albedo[7 * B - 17, 16 * B + 7 : 16 * B + 9] = rgb(214, 60, 60)
LANTERN = (18 * B + 2, 7 * B - 12, 18 * B + 10, 7 * B - 4)
albedo[LANTERN[1] : LANTERN[3], LANTERN[0] : LANTERN[2]] = rgb(56, 56, 64)
albedo[LANTERN[1] + 2 : LANTERN[3] - 1, LANTERN[0] + 2 : LANTERN[2] - 2] = rgb(255, 214, 120)
# A red rug on the floor in front of the fire.
albedo[10 * B + 2 : 10 * B + 14, 12 * B : 19 * B] = rgb(104, 30, 30)
albedo[10 * B + 4 : 10 * B + 12, 12 * B + 2 : 19 * B - 2] = rgb(124, 40, 34)
albedo[10 * B + 7 : 10 * B + 9, 12 * B + 4 : 19 * B - 4] = rgb(160, 120, 56)
albedo[10 * B + 2 : 10 * B + 14, 12 * B : 19 * B] = jitter(albedo[10 * B + 2 : 10 * B + 14, 12 * B : 19 * B], 6)

# Glowing pixels: fully bright regardless of light.
emissive = np.zeros(lit.shape, bool)
emissive[wy0:wy1, wx0:wx1] = albedo[wy0:wy1, wx0:wx1].mean(-1) > 0.8  # moon
emissive[LANTERN[1] + 2 : LANTERN[3] - 1, LANTERN[0] + 2 : LANTERN[2] - 2] = True


# ------------------------------------------------------------------- light
yy, xx = np.mgrid[0 : ROWS * B, 0:W].astype(np.float32)
FIRE_CX, FIRE_CY = (ox0 + ox1) / 2, oy1 - 10
fire_d = np.hypot((xx - FIRE_CX) * 0.85, yy - FIRE_CY)
lantern_d = np.hypot(xx - (LANTERN[0] + LANTERN[2]) / 2, yy - (LANTERN[1] + LANTERN[3]) / 2)
moon_d = np.hypot((xx - (wx0 + wx1) / 2) * 0.7, yy - (wy0 + wy1) / 2)
AMBIENT = rgb(58, 62, 92) * 1.1
FIRE_LIGHT = rgb(255, 168, 92)
MOON_LIGHT = rgb(110, 130, 190)
LEVELS = 14  # light comes in steps, like block light levels


def light(t):
    """Light on each pixel in frame t: ambient, moon, lantern and the fire,
    which flickers (periodic in FRAMES, so the loop has no seam)."""
    ph = 2 * np.pi * t / FRAMES
    flicker = 1 + 0.07 * np.sin(ph) + 0.05 * np.sin(3 * ph + 1.3) + 0.03 * np.sin(7 * ph + 0.4)
    fire = np.clip(1 - fire_d / (230 * flicker), 0, 1) ** 1.6 * 1.35 * flicker
    lantern = np.clip(1 - lantern_d / 60, 0, 1) ** 2 * 0.8
    moon = np.clip(1 - moon_d / 90, 0, 1) ** 2 * 0.6
    steps = lambda a: np.floor(a * LEVELS) / LEVELS
    l = AMBIENT + steps(fire)[..., None] * FIRE_LIGHT + steps(lantern)[..., None] * FIRE_LIGHT
    l = l + steps(moon)[..., None] * MOON_LIGHT
    return l * lit[..., None] + (1 - lit[..., None])


# -------------------------------------------------------------------- fire
FW, FH = ox1 - ox0, 30  # fire box, its bottom on the logs
fire_noise = gaussian_filter(rng.random((FH, FW)), (1.2, 0.9), mode="wrap")
fire_noise = (fire_noise - fire_noise.min()) / np.ptp(fire_noise)
fine = gaussian_filter(rng.random((FH, FW)), 0.6, mode="wrap")
fine = (fine - fine.min()) / np.ptp(fine)
fx = np.arange(FW)
# Taller in the middle of each log pile, tapering at the sides.
envelope = 0.55 + 0.45 * np.cos((fx - FW / 2) / FW * np.pi) ** 2
envelope *= 1 + 0.15 * np.sin(fx / 3.0)
FIRE_PAL = [
    (0.20, rgb(150, 34, 16)),
    (0.38, rgb(220, 90, 24)),
    (0.56, rgb(252, 160, 40)),
    (0.74, rgb(255, 214, 90)),
    (0.88, rgb(255, 246, 190)),
]


def fire(t):
    """Fire pixels for frame t: noise scrolling up (one period per loop)
    shaped into a flame, then cut into Minecraft-like colour bands."""
    s1 = int(round(t * FH / FRAMES))
    s2 = int(round(t * 2 * FH / FRAMES))
    n = np.roll(fire_noise, -s1, 0) * 0.7 + np.roll(fine, -s2, 0) * 0.3
    h = 1 - np.arange(FH)[::-1, None] / (FH * envelope[None, :])  # 1 at the bottom
    heat = np.clip(h, 0, 1) * 1.15 + (n - 0.5) * 0.9 - 0.12
    out = np.zeros((FH, FW, 3), np.float32)
    mask = np.zeros((FH, FW), bool)
    for level, c in FIRE_PAL:
        on = heat > level
        out[on] = c
        mask |= on
    return out, mask


def frame(t):
    img = albedo * light(t)
    img[emissive] = albedo[emissive]
    # Stars twinkle out of step with each other.
    for (x, y), p in zip(stars, star_phase):
        on = ((t + p) % FRAMES) < FRAMES * 0.8
        img[y, x] = rgb(240, 240, 255) if on else rgb(90, 100, 150)
    # The candle's flame, two pixels that sway.
    cx, cy = CANDLE
    img[cy - 3 + (t % 4 == 0), cx + (t % 6 == 3)] = rgb(255, 210, 90)
    img[cy - 2, cx] = rgb(255, 240, 170)
    f, m = fire(t)
    region = img[oy1 - 6 - FH : oy1 - 6, ox0:ox1]
    region[m] = f[m]
    # Embers under the logs.
    embers = img[oy1 - 4 : oy1, ox0 + 4 : ox1 - 4]
    glow = (np.roll(fine[:4, 4:-4], t, 1) > 0.55)
    embers[glow] = rgb(255, 120, 30)
    # Sparks drifting up out of the fire, each on a loop.
    for i in range(6):
        p = ((t + i * FRAMES // 6) % FRAMES) / FRAMES
        sx = int(FIRE_CX + 14 * np.sin(i * 2.1 + p * 4) + (i - 3) * 4)
        sy = int(oy1 - 14 - p * 34)
        if oy0 - 2 < sy < oy1 and ox0 < sx < ox1:
            img[sy, sx] = rgb(255, 200, 80)
    return np.clip(img[:H], 0, 1)


frames = [(frame(t) * 255 + 0.5).astype(np.uint8) for t in range(FRAMES)]
# One palette for all frames (GIF's 256 colours), no dithering: flat pixels.
sheet = Image.fromarray(np.concatenate(frames, 0))
pal = sheet.quantize(colors=255, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
gif = [Image.fromarray(f).quantize(palette=pal, dither=Image.Dither.NONE) for f in frames]
RAW.mkdir(parents=True, exist_ok=True)
THUMBS.mkdir(parents=True, exist_ok=True)
for path in (RAW / "bg_fireplace.gif", THUMBS / "fireplace.gif"):
    gif[0].save(path, save_all=True, append_images=gif[1:], duration=1000 // FPS, loop=0, optimize=False, disposal=1)
print("fireplace", (RAW / "bg_fireplace.gif").stat().st_size, "bytes")
