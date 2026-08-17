#!/usr/bin/env python3
"""Generate the Lantern wordmark and lockups as pure-path SVGs.

Text is converted to outlines from Outfit (OFL), so the mark renders
identically everywhere with no font dependency. Re-run after changing
tracking or weight.
"""
import os
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen

HERE = os.path.dirname(os.path.abspath(__file__))
FONT = os.path.join(HERE, "fonts", "Outfit-Bold.ttf")
TEXT = "Lantern"
TRACKING = -8  # font units; slightly tight, confident

font = TTFont(FONT)
glyph_set = font.getGlyphSet()
cmap = font.getBestCmap()
upem = font["head"].unitsPerEm
ascent, descent = font["hhea"].ascent, font["hhea"].descent

paths, x = [], 0
for ch in TEXT:
    gname = cmap[ord(ch)]
    glyph = glyph_set[gname]
    pen = SVGPathPen(glyph_set)
    glyph.draw(pen)
    d = pen.getCommands()
    if d:
        paths.append((d, x))
    x += glyph.width + TRACKING

width_units = x - TRACKING
H = 200  # output height for the type box
scale = H / (ascent - descent)
W = width_units * scale

# y-flip: font coords are y-up, SVG y-down; baseline at ascent*scale.
body = "\n".join(
    f'  <path transform="translate({dx * scale:.2f},{ascent * scale:.2f}) '
    f'scale({scale:.6f},{-scale:.6f})" d="{d}"/>'
    for d, dx in paths
)

svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="{W:.0f}" height="{H}"
     viewBox="0 0 {W:.2f} {H}" fill="currentColor">
{body}
</svg>
'''
with open(os.path.join(HERE, "wordmark.svg"), "w") as f:
    f.write(svg)
print(f"wordmark.svg: {W:.0f}x{H} ({len(paths)} glyphs)")

# Horizontal lockup: laltain mark + wordmark, baseline-aligned.
icon_svg = open(os.path.join(HERE, "..", "icon", "lantern.svg")).read()
icon_inner = icon_svg[icon_svg.index("<defs"):icon_svg.rindex("</svg>")]
ICON = 200
GAP = 56
LW = ICON + GAP + W * (140 / H)  # wordmark scaled to 140 tall
lockup = f'''<svg xmlns="http://www.w3.org/2000/svg" width="{LW:.0f}" height="{ICON}"
     viewBox="0 0 {LW:.2f} {ICON}">
  <g transform="scale({ICON / 1024:.6f})">
{icon_inner}
  </g>
  <g transform="translate({ICON + GAP},{(ICON - 140) / 2:.0f}) scale({140 / H:.4f})"
     fill="#eef1f6">
{body}
  </g>
</svg>
'''
with open(os.path.join(HERE, "lockup-horizontal.svg"), "w") as f:
    f.write(lockup)
print(f"lockup-horizontal.svg: {LW:.0f}x{ICON}")
