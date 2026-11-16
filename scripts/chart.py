"""Draw the README's benchmark chart from docs/bench.json.

Usage: python scripts/chart.py

Writes docs/bench-light.svg and docs/bench-dark.svg: one row per command, with radon's time as a
bar on a linear axis, oxradon's time as a mark at the start of that bar, and how many times
faster oxradon is. oxradon's mark is drawn MARK pixels wide so that it can be seen; at true scale
it would be thinner than a pixel. `speedup` is stored in the JSON, not recomputed here, because
the stored times are rounded.
"""

import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOCS = os.path.join(ROOT, "docs")

THEMES = {
    "light": {"text": "#1F1B16", "muted": "#6B6257", "radon": "#CFC7B8", "ox": "#C4501B"},
    "dark": {"text": "#EDE6D9", "muted": "#A39A8C", "radon": "#4A433B", "ox": "#EE7A45"},
}

WIDTH = 760
BAR_X = 56
BAR_MAX = 330
BAR_H = 18
MARK = 3
ROW_H = 44
TOP = 34
COL_RADON = 500
COL_OX = 600
COL_SPEED = WIDTH
FONT = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"


def human(seconds):
    """A time as a reader would say it: 0.164 -> '0.16 s', 166 -> '2 min 46 s'."""
    if seconds < 1:
        return f"{seconds:.2f} s"
    if seconds < 60:
        return f"{seconds:.1f} s"
    minutes, rest = divmod(round(seconds), 60)
    return f"{minutes} min {rest} s"


def text(x, y, body, fill, anchor="start", size=14, weight=400):
    return (
        f'<text x="{x}" y="{y}" fill="{fill}" text-anchor="{anchor}" font-size="{size}" '
        f'font-weight="{weight}">{body}</text>'
    )


def draw(data, colors):
    rows = data["rows"]
    longest = max(row["radon_s"] for row in rows)
    height = TOP + ROW_H * len(rows)
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {height}" '
        f'width="{WIDTH}" height="{height}" role="img" font-family="{FONT}">',
        "<title>Time to analyze 3,824 Python files: radon and oxradon</title>",
        text(COL_RADON, 14, "radon", colors["muted"], "end", 13),
        text(COL_OX, 14, "oxradon", colors["ox"], "end", 13),
    ]
    for i, row in enumerate(rows):
        y = TOP + i * ROW_H
        base = y + 14
        width = row["radon_s"] / longest * BAR_MAX
        out += [
            text(0, base, row["command"], colors["text"], size=15, weight=600),
            f'<rect x="{BAR_X}" y="{y}" width="{width:.1f}" height="{BAR_H}" rx="2" '
            f'fill="{colors["radon"]}"/>',
            f'<rect x="{BAR_X}" y="{y - 3}" width="{MARK}" height="{BAR_H + 6}" '
            f'fill="{colors["ox"]}"/>',
            text(COL_RADON, base, human(row["radon_s"]), colors["muted"], "end"),
            text(COL_OX, base, human(row["oxradon_s"]), colors["ox"], "end"),
            text(COL_SPEED, base + 2, f'{row["speedup"]}x faster', colors["ox"], "end", 19, 700),
        ]
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    with open(os.path.join(DOCS, "bench.json"), encoding="utf-8") as f:
        data = json.load(f)
    for theme, colors in THEMES.items():
        target = os.path.join(DOCS, f"bench-{theme}.svg")
        with open(target, "w", encoding="utf-8") as f:
            f.write(draw(data, colors))
        print(f"wrote {target}")


if __name__ == "__main__":
    main()
