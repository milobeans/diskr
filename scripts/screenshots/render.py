"""Render pyte screen grids as macOS-style terminal windows.

Box-drawing and block glyphs are drawn as geometry rather than font glyphs so
borders connect and bars fill cells exactly, as a native terminal draws them.
"""
import functools
import os
import re
import subprocess

from fontTools.ttLib import TTCollection, TTFont
from PIL import Image, ImageDraw, ImageFilter, ImageFont

FONT = "/System/Library/Fonts/SFNSMono.ttf"
FALLBACKS = [
    ("/System/Library/Fonts/Menlo.ttc", 0),
    ("/System/Library/Fonts/Apple Symbols.ttf", 0),
    ("/System/Library/Fonts/Apple Braille.ttf", 0),
]
UI_FONT = "/System/Library/Fonts/SFNS.ttf"
REQUIRED_FONTS = [FONT, UI_FONT] + [path for path, _ in FALLBACKS]

# Catppuccin Mocha-inspired palette, keyed by the xterm hex values pyte reports
# for diskr's ANSI and 256-color indexes.
BG = (30, 30, 46)
FG = (205, 214, 244)
PALETTE = {
    "000000": (24, 24, 37), "cd0000": (243, 139, 168), "00cd00": (166, 227, 161),
    "cdcd00": (249, 226, 175), "0000ee": (137, 180, 250), "cd00cd": (203, 166, 247),
    "00cdcd": (148, 226, 213), "e5e5e5": (186, 194, 222), "7f7f7f": (108, 112, 134),
    "ff0000": (243, 139, 168), "00ff00": (166, 227, 161), "ffff00": (249, 226, 175),
    "5c5cff": (137, 180, 250), "ff00ff": (245, 194, 231), "00ffff": (137, 220, 235),
    "ffffff": (230, 233, 245), "949494": (140, 146, 172), "262626": (49, 50, 68),
    "bcbcbc": (166, 173, 200), "303030": (49, 50, 68), "3a3a3a": (69, 71, 90),
}

# (left, right, up, down) segments for light box-drawing characters.
BOX = {
    "─": "LR", "│": "UD", "┌": "RD", "┐": "LD", "└": "RU", "┘": "LU", "├": "UDR",
    "┤": "UDL", "┬": "LRD", "┴": "LRU", "┼": "LRUD", "╭": "RD", "╮": "LD", "╰": "RU",
    "╯": "LU", "╴": "L", "╶": "R", "╵": "U", "╷": "D",
}
SHADES = {"░": 0.28, "▒": 0.5, "▓": 0.75}
TITLE = "diskr — ~"


class Masker:
    """Replace host paths with neutral demo paths, keeping columns aligned."""

    def __init__(self, pairs, user):
        # Longest first so nested paths (the demo home inside the real home) win.
        self.pairs = sorted(pairs, key=lambda pair: len(pair[0]), reverse=True)
        self.user = user if len(user) >= 5 else ""
        self.user_pattern = re.compile(rf"/{re.escape(user)}\b") if user else None

    def apply(self, cells):
        cells = [[list(c) for c in row] for row in cells]
        for old, new in self.pairs:
            for row in cells:
                replace_in_row(row, old, new)
        # diskr shortens long paths from the start ("…tail"); swap a visible
        # tail of a host path for the shown path.
        for old, new in self.pairs:
            for row in cells:
                for k in range(len(old) - 1, 7, -1):
                    if "…" + old[-k:] in row_text(row):
                        replace_in_row(row, "…" + old[-k:], new)
                        break
        # Overlays can also cut a path short at the end; mask that prefix in place.
        # Shared prefixes belong to the nearest known parent (usually HOME),
        # not an arbitrary deeper path such as the synthetic Homebrew Cellar.
        for old, new in reversed(self.pairs):
            for row in cells:
                text = row_text(row)
                for k in range(len(old) - 1, 7, -1):
                    at = text.find(old[:k])
                    if at >= 0:
                        for i, ch in enumerate((new + " " * k)[:k]):
                            row[at + i][0] = ch
                        break
        return cells

    def leaks(self, cells):
        found = []
        for row in cells:
            text = row_text(row)
            tails = (old[-12:] for old, new in self.pairs if len(old) > 12 and old[-12:] not in new)
            if any(old in text for old, _ in self.pairs) or any(t in text for t in tails) or (
                    self.user and self.user in text) or (
                    self.user_pattern and self.user_pattern.search(text)):
                found.append(text.rstrip())
        return found


def row_text(row):
    return "".join(c[0][:1] if c[0] else "\0" for c in row)


def replace_in_row(row, old, new):
    if not old:
        return
    new = new[:len(old)]
    cursor = 0
    while True:
        at = row_text(row).find(old, cursor)
        if at < 0:
            return
        for i, ch in enumerate(new):
            row[at + i][0] = ch
        removed = len(old) - len(new)
        del row[at + len(new):at + len(old)]
        # Pad inside the next run of repeated spaces or border lines so later
        # columns, and box borders, keep their positions.
        gap = next((j for j in range(at + len(new), len(row) - 1)
                    if row[j][0] in (" ", "─") and row[j][0] == row[j + 1][0]), len(row) - 1)
        for _ in range(removed):
            row.insert(gap, list(row[gap]))
        cursor = at + len(new)


def cmap_of(path, index=0):
    if path.endswith(".ttc"):
        return set(TTCollection(path)[index].getBestCmap())
    return set(TTFont(path).getBestCmap())


@functools.lru_cache(maxsize=None)
def fonts(scale):
    size = round(14 * scale)
    regular = ImageFont.truetype(FONT, size)
    regular.set_variation_by_name("Regular")
    bold = ImageFont.truetype(FONT, size)
    bold.set_variation_by_name("Semibold")
    fallbacks = [(ImageFont.truetype(p, size, index=i), cmap_of(p, i)) for p, i in FALLBACKS]
    return size, regular, bold, cmap_of(FONT), fallbacks


def color(value, is_bg):
    if value == "default" or (is_bg and value == "000000"):
        return BG if is_bg else FG
    if value in PALETTE:
        return PALETTE[value]
    if len(value) == 6:
        return tuple(int(value[i:i + 2], 16) for i in (0, 2, 4))
    return FG


def blend(a, b, t):
    return tuple(round(a[i] * t + b[i] * (1 - t)) for i in range(3))


def terminal_image(cells, cols, rows, scale):
    """Draw already-masked cells inside a window with a soft shadow."""
    size, regular, bold, primary, fallbacks = fonts(scale)
    cw = regular.getlength("0")
    ch = round(size * 1.32)
    ascent, descent = regular.getmetrics()
    base_off = (ch - (ascent + descent)) / 2 + ascent

    pad_x, pad_top, pad_bottom = round(16 * scale), round(10 * scale), round(14 * scale)
    bar_h = round(30 * scale)
    win_w = round(cols * cw) + 2 * pad_x
    win_h = bar_h + pad_top + rows * ch + pad_bottom
    margin = round(34 * scale)
    radius = round(11 * scale)

    canvas = Image.new("RGBA", (win_w + 2 * margin, win_h + 2 * margin), (0, 0, 0, 0))
    shadow = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    off = round(10 * scale)
    ImageDraw.Draw(shadow).rounded_rectangle(
        [margin, margin + off, margin + win_w, margin + win_h + off // 2],
        radius=radius, fill=(0, 0, 0, 120))
    canvas.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(round(16 * scale))))

    mask = Image.new("L", (win_w, win_h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, win_w - 1, win_h - 1], radius=radius, fill=255)
    body = Image.new("RGBA", (win_w, win_h), BG + (255,))
    d = ImageDraw.Draw(body)
    d.rectangle([0, 0, win_w, bar_h], fill=(24, 24, 37, 255))
    d.line([0, bar_h, win_w, bar_h], fill=(17, 17, 27, 255), width=max(1, round(scale / 2)))
    r = round(6 * scale)
    for i, light in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
        cx, cy = round(20 * scale) * (i + 1), bar_h // 2
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=light + (255,))
    ui = ImageFont.truetype(UI_FONT, round(13 * scale))
    ui.set_variation_by_name("Medium")
    d.text((win_w / 2, bar_h / 2), TITLE, font=ui, fill=(147, 153, 178, 255), anchor="mm")

    ox, oy = pad_x, bar_h + pad_top
    t = max(1, round(scale * 1.5))
    inset = round(ch * 0.18)
    for y, row in enumerate(cells):
        top = oy + y * ch
        for x, (text, fg, bg, is_bold, reverse) in enumerate(row):
            fgc, bgc = color(fg, False), color(bg, True)
            if reverse:
                fgc, bgc = bgc, fgc
            x0, x1 = round(ox + x * cw), round(ox + (x + 1) * cw)
            if bgc != BG:
                d.rectangle([x0, top, x1 - 1, top + ch - 1], fill=bgc)
            if not text or text == " ":
                continue
            glyph = text[0]
            if glyph in BOX:
                cx, cy = (x0 + x1) // 2, top + ch // 2
                h0, h1 = cx - t // 2, cx - t // 2 + t - 1
                v0, v1 = cy - t // 2, cy - t // 2 + t - 1
                segs = BOX[glyph]
                if "L" in segs:
                    d.rectangle([x0, v0, h1, v1], fill=fgc)
                if "R" in segs:
                    d.rectangle([h0, v0, x1 - 1, v1], fill=fgc)
                if "U" in segs:
                    d.rectangle([h0, top, h1, v1], fill=fgc)
                if "D" in segs:
                    d.rectangle([h0, v0, h1, top + ch - 1], fill=fgc)
            elif glyph == "█" or glyph in SHADES:
                fill = fgc if glyph == "█" else blend(fgc, bgc, SHADES[glyph])
                d.rectangle([x0, top + inset, x1 - 1, top + ch - 1 - inset], fill=fill)
            else:
                font = bold if is_bold else regular
                if ord(glyph) not in primary:
                    font = next((f for f, cm in fallbacks if ord(glyph) in cm), regular)
                d.text((x0, top + base_off), text, font=font, fill=fgc, anchor="ls")

    d.rounded_rectangle([0, 0, win_w - 1, win_h - 1], radius=radius,
                        outline=(255, 255, 255, 28), width=max(1, round(scale / 2)))
    win = Image.new("RGBA", (win_w, win_h), (0, 0, 0, 0))
    win.paste(body, (0, 0), mask)
    canvas.alpha_composite(win, (margin, margin))
    return canvas


def animate(frames, cols, rows, scale, out, work):
    """Encode masked frames as a looping lossless WebP; long holds are capped."""
    os.makedirs(work, exist_ok=True)
    args = ["img2webp", "-loop", "0", "-lossless", "-m", "4"]
    for i, frame in enumerate(frames):
        nxt = frames[i + 1]["t"] if i + 1 < len(frames) else frame["t"] + 3.0
        ms = min(2400, max(40, round((nxt - frame["t"]) * 1000)))
        path = os.path.join(work, f"{i:04d}.png")
        terminal_image(frame["cells"], cols, rows, scale).save(path)
        args += ["-d", str(ms), path]
    subprocess.run(args + ["-o", out], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def social_card(screenshot, out):
    """1280x640 repository social preview: wordmark, tagline, screenshot."""
    s = 2
    w, h = 1280 * s, 640 * s
    card = Image.new("RGBA", (w, h), (17, 17, 27, 255))
    grad = ImageDraw.Draw(card)
    for y in range(h):
        t = y / h
        grad.line([(0, y), (w, y)], fill=(round(17 + 13 * t), round(17 + 13 * t), round(27 + 19 * t), 255))
    glow = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse([w * 0.2, h * 0.45, w * 0.8, h * 1.25], fill=(137, 180, 250, 60))
    card.alpha_composite(glow.filter(ImageFilter.GaussianBlur(120 * s)))

    d = ImageDraw.Draw(card)
    mono = ImageFont.truetype(FONT, 92 * s)
    mono.set_variation_by_name("Bold")
    ui = ImageFont.truetype(UI_FONT, 30 * s)
    d.text((w / 2, 92 * s), "diskr", font=mono, fill=(205, 214, 244, 255), anchor="mm")
    d.text((w / 2, 168 * s), "See where your disk space went, and what you can safely take back.",
           font=ui, fill=(166, 173, 200, 255), anchor="mm")

    shot = Image.open(screenshot).convert("RGBA")
    target_w = 1180 * s
    shot = shot.resize((target_w, round(shot.height * target_w / shot.width)), Image.LANCZOS)
    card.alpha_composite(shot, ((w - target_w) // 2, 205 * s))
    card.resize((1280, 640), Image.LANCZOS).convert("RGB").save(out, optimize=True)


def replay_json(frames, stills, chapters, cols, rows, out):
    """Compact replay for the docs site: a style table, deduplicated rows and
    per-frame hold times. Only the first `play` frames are played; named stills
    for annotated figures follow them. `chapters` maps names to frame indexes."""
    import json

    styles, style_ids, lines, line_ids, packed = [], {}, [], {}, []

    def pack(cells):
        ids = []
        for row in cells:
            runs = []
            for ch, fg, bg, bold, rev in row:
                key = (fg, bg, bool(bold), bool(rev))
                if key not in style_ids:
                    style_ids[key] = len(styles)
                    styles.append(list(key))
                ch = ch or "\u0000"  # trailing half of a wide glyph
                if runs and runs[-1][1] == style_ids[key]:
                    runs[-1][0] += ch
                else:
                    runs.append([ch, style_ids[key]])
            key = json.dumps(runs, ensure_ascii=False)
            if key not in line_ids:
                line_ids[key] = len(lines)
                lines.append(runs)
            ids.append(line_ids[key])
        return ids

    for i, frame in enumerate(frames):
        nxt = frames[i + 1]["t"] if i + 1 < len(frames) else frame["t"] + 3.0
        packed.append({"d": min(2400, max(40, round((nxt - frame["t"]) * 1000))), "l": pack(frame["cells"])})
    marks = {}
    for name, cells in stills:
        marks[name] = len(packed)
        packed.append({"d": 0, "l": pack(cells)})
    data = {"cols": cols, "rows": rows, "styles": styles, "lines": lines, "frames": packed,
            "play": len(frames), "chapters": [{"name": n, "frame": i} for n, i in chapters],
            "marks": marks}
    with open(out, "w") as f:
        json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
