"""Render the UI cell dumps into the PNG screenshots used in the README.

From the repository root:

    cargo test dump_screens_for_screenshots -- --ignored --nocapture
    pip install pillow
    python assets/render-screenshots.py

The (ignored) test draws the real interface through ratatui's TestBackend
with a representative snapshot and dumps every cell — glyph, colors and
modifiers — into `target/screenshots/`. This script paints those dumps into
PNGs in `assets/screenshots/`. The screenshots therefore always show the
actual interface code, never a mock-up; only the data is sample data.
"""
import re
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

SRC = Path("target/screenshots")
OUT = Path("assets/screenshots")
OUT.mkdir(parents=True, exist_ok=True)

FONT_SIZE = 22
CELL_W = 13
CELL_H = 26
PAD = 28
BG = (13, 17, 23)          # GitHub-dark style canvas
TERM_BG = (10, 12, 16)     # default cell background

# A monospace font that covers the block/sparkline glyphs (Cascadia Mono on
# Windows, DejaVu Sans Mono on Linux, Menlo on macOS).
_FONT_CANDIDATES = [
    (r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\consolab.ttf"),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
     "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"),
    ("/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/Menlo.ttc"),
]


def _load_fonts():
    for regular_path, bold_path in _FONT_CANDIDATES:
        if Path(regular_path).exists():
            regular = ImageFont.truetype(regular_path, FONT_SIZE)
            bold = (ImageFont.truetype(bold_path, FONT_SIZE)
                    if Path(bold_path).exists() else regular)
            return regular, bold
    default = ImageFont.load_default()
    return default, default


regular, bold = _load_fonts()

NAMED = {
    "Reset": None,
    "Black": (0, 0, 0),
    "White": (255, 255, 255),
}


def parse_color(text):
    m = re.fullmatch(r"Rgb\((\d+), (\d+), (\d+)\)", text)
    if m:
        return tuple(int(v) for v in m.groups())
    return NAMED.get(text, (200, 200, 200))


def render(path: Path):
    lines = path.read_text(encoding="utf-8").splitlines()
    width, height = (int(v) for v in lines[0].split())
    cells = [line.split("\t") for line in lines[1:]]
    assert len(cells) == width * height, (path, len(cells))

    img_w = width * CELL_W + PAD * 2
    img_h = height * CELL_H + PAD * 2
    img = Image.new("RGB", (img_w, img_h), BG)
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(
        [PAD // 2, PAD // 2, img_w - PAD // 2, img_h - PAD // 2],
        radius=14, fill=TERM_BG, outline=(48, 54, 61), width=2,
    )

    for idx, (symbol, fg, bg, modifier) in enumerate(cells):
        x = PAD + (idx % width) * CELL_W
        y = PAD + (idx // width) * CELL_H
        bg_color = parse_color(bg)
        if bg_color is not None:
            d.rectangle([x, y, x + CELL_W, y + CELL_H], fill=bg_color)
        if symbol.strip() == "":
            continue
        fg_color = parse_color(fg) or (201, 209, 217)
        font = bold if "BOLD" in modifier else regular
        d.text((x, y + 1), symbol, font=font, fill=fg_color)
        if "CROSSED_OUT" in modifier:
            mid = y + CELL_H // 2
            d.line([x, mid, x + CELL_W, mid], fill=fg_color, width=2)

    out = OUT / (path.stem + ".png")
    img.save(out, optimize=True)
    print(out, img.size)


for cells_file in sorted(SRC.glob("*.cells")):
    render(cells_file)
