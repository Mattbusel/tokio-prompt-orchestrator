"""Render TestBackend cell dumps (frames.jsonl) to PNG frames."""
import json, os, sys
from PIL import Image, ImageDraw, ImageFont

SRC = sys.argv[1] if len(sys.argv) > 1 else "frames.jsonl"
OUT = sys.argv[2] if len(sys.argv) > 2 else "png"
os.makedirs(OUT, exist_ok=True)
SIZE = 16
reg = ImageFont.truetype("C:/Windows/Fonts/CascadiaMono.ttf", SIZE)
bold = ImageFont.truetype("C:/Windows/Fonts/CascadiaMono.ttf", SIZE)
try:
    bold.set_variation_by_name("Bold")
except Exception:
    pass
asc, desc = reg.getmetrics()
CW = round(reg.getlength("M"))
CH = asc + desc
PAD = 14
FG = {"Reset": "#c9d1d9", "White": "#f0f3f6", "Red": "#ff6b6b", "Green": "#3fd26b",
      "Yellow": "#f0c040", "Cyan": "#56c8e0", "DarkGray": "#6e7681", "Gray": "#9aa4ad",
      "Blue": "#4c9aff", "Magenta": "#d27ce8", "LightRed": "#ff8f8f", "LightGreen": "#6ee79b"}
BG = "#0d1117"

for i, line in enumerate(open(SRC, encoding="utf-8")):
    d = json.loads(line)
    w, h = d["w"], d["h"]
    img = Image.new("RGB", (w * CW + 2 * PAD, h * CH + 2 * PAD), BG)
    dr = ImageDraw.Draw(img)
    for idx, (sym, fg, bgc, mod) in enumerate(d["cells"]):
        r, c = divmod(idx, w)
        x, y = PAD + c * CW, PAD + r * CH
        if bgc not in ("Reset",):
            dr.rectangle([x, y, x + CW - 1, y + CH - 1], fill=FG.get(bgc, BG))
        if sym.strip() == "":
            continue
        col = FG.get(fg, FG["Reset"])
        o = ord(sym[0])
        # Draw block elements as rectangles so bars and sparklines have no gaps.
        if o == 0x2588:
            dr.rectangle([x, y, x + CW - 1, y + CH - 1], fill=col)
        elif 0x2581 <= o <= 0x2587:
            frac = (o - 0x2580) / 8
            dr.rectangle([x, y + CH - round(CH * frac), x + CW - 1, y + CH - 1], fill=col)
        elif o == 0x2591:
            for yy in range(y, y + CH, 3):
                for xx in range(x + (yy // 3) % 2, x + CW, 3):
                    dr.point((xx, yy), fill=col)
        elif o in (0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524, 0x252C, 0x2534, 0x253C):
            cx, cy = x + CW // 2, y + CH // 2
            L = o in (0x2500, 0x2510, 0x2518, 0x2524, 0x252C, 0x2534, 0x253C)
            R = o in (0x2500, 0x250C, 0x2514, 0x251C, 0x252C, 0x2534, 0x253C)
            U = o in (0x2502, 0x2514, 0x2518, 0x251C, 0x2524, 0x2534, 0x253C)
            D = o in (0x2502, 0x250C, 0x2510, 0x251C, 0x2524, 0x252C, 0x253C)
            if L: dr.line([x, cy, cx, cy], fill=col)
            if R: dr.line([cx, cy, x + CW, cy], fill=col)
            if U: dr.line([cx, y, cx, cy], fill=col)
            if D: dr.line([cx, cy, cx, y + CH], fill=col)
        else:
            dr.text((x, y), sym, font=bold if mod & 1 else reg, fill=col)
    img.save(f"{OUT}/f{i:03d}.png")
print("cell", CW, CH, "frames", i + 1, "size", img.size)
