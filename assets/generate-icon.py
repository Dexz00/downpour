"""Generate `downpour.ico` from `logo.png`.

    python assets/generate-icon.py

The logo is wide and has text underneath; an icon is square and will be seen
at 16 pixels. So the icon is not a plain crop: the folder is isolated, the
text touching the footer is erased, the green glow is rebuilt around it and
the small sizes use a tighter framing - the whole drawing shrunk to 16
pixels would be a blur.

Depends on Pillow (`pip install pillow`). The `.ico` is versioned, so running
this is only needed when the logo changes.
"""

from PIL import Image, ImageDraw, ImageFilter, ImageChops
from pathlib import Path
import io
import struct

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "logo.png"
DEST = HERE / "downpour.ico"

# Coordinates in the original logo (1536x1024).
WIDE = (488, 148, 1018, 548)    # the whole folder: tab, arrow and symbol
TIGHT = (545, 262, 900, 550)    # just the arrow and symbol, for 16-24 px
ARROW = (533, 718)              # x range occupied by the arrow
FADE_FROM, FADE_TO = 512, 548   # where the footer (top of the text) fades out
SIDE = 1024
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)


def crop(source, box):
    """Crop and erase the text creeping into the footer, sparing the arrow column."""
    cut = source.crop(box)
    w, h = cut.size

    mask = Image.new("L", (w, h), 255)
    d = ImageDraw.Draw(mask)
    y0, y1 = FADE_FROM - box[1], FADE_TO - box[1]
    for i in range(max(0, y0), h):
        v = 0 if i >= y1 else int(255 * (1 - (i - y0) / (y1 - y0)))
        d.line([(0, i), (w, i)], fill=v)
    d.rectangle([ARROW[0] - box[0], 0, ARROW[1] - box[0], h], fill=255)
    return Image.composite(cut, Image.new("RGB", (w, h), (0, 0, 0)), mask)


def compose(cut, fill=0.99):
    """Center on a black square and give the background its green glow back."""
    w, h = cut.size
    scale = (SIDE * fill) / max(w, h)
    new = (int(w * scale), int(h * scale))
    cut = cut.resize(new, Image.LANCZOS)
    px, py = (SIDE - new[0]) // 2, (SIDE - new[1]) // 2

    glow = Image.new("RGB", (SIDE, SIDE), (0, 0, 0))
    glow.paste(cut, (px, py))
    glow = glow.filter(ImageFilter.GaussianBlur(SIDE // 12))
    r, g, b = glow.split()
    canvas = ImageChops.add(
        Image.new("RGB", (SIDE, SIDE), (0, 0, 0)),
        Image.merge("RGB", (r.point(lambda v: v // 3), g, b.point(lambda v: v // 3))),
    )
    canvas.paste(cut, (px, py))
    return canvas


def dib(image):
    """32-bit bitmap with a zeroed AND mask: the format the old shell reads."""
    t = image.size[0]
    px = image.load()
    rows = []
    for y in range(t - 1, -1, -1):                  # the DIB is stored bottom-up
        row = bytearray()
        for x in range(t):
            r, g, b, a = px[x, y]
            row += bytes((b, g, r, a))
        rows.append(bytes(row))
    header = struct.pack("<IiiHHIIiiII", 40, t, t * 2, 1, 32, 0, t * t * 4, 0, 0, 0, 0)
    mask_row = ((t + 31) // 32) * 4                 # each row aligned to 4 bytes
    return header + b"".join(rows) + b"\x00" * (mask_row * t)


def main():
    source = Image.open(SOURCE).convert("RGB")
    wide = compose(crop(source, WIDE))
    tight = compose(crop(source, TIGHT))

    entries = []
    for t in SIZES:
        base = tight if t <= 24 else wide
        p = base.resize((t, t), Image.LANCZOS).convert("RGBA")
        if t >= 64:                                 # PNG only in the large ones
            buf = io.BytesIO()
            p.save(buf, format="PNG", optimize=True)
            entries.append((t, buf.getvalue()))
        else:
            entries.append((t, dib(p)))

    with open(DEST, "wb") as f:
        f.write(struct.pack("<HHH", 0, 1, len(entries)))
        pos = 6 + 16 * len(entries)
        for t, data in entries:
            # width and height are one byte: 0 means 256.
            f.write(struct.pack("<BBBBHHII", t % 256, t % 256, 0, 0, 1, 32, len(data), pos))
            pos += len(data)
        for _, data in entries:
            f.write(data)

    print(f"{DEST.name}: {len(entries)} sizes, {DEST.stat().st_size} bytes")


if __name__ == "__main__":
    main()
