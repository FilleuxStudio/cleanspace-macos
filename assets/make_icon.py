#!/usr/bin/env python3
"""Génère l'icône de CleanSpace (dessin original, aucune ressource externe).

    python3 assets/make_icon.py

Produit, dans le dossier assets/ :
  - icon.png         1024x1024, source de référence (README, fenêtre de l'app)
  - icon-64.png      logo de l'en-tête de l'interface
  - CleanSpace.icns  icône macOS (toutes les tailles, via Pillow)

Dépendance : Pillow (pip install pillow).
"""

import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

OUT = Path(__file__).resolve().parent
SS = 2  # sur-échantillonnage pour des bords lisses
SIZE = 1024
W = SIZE * SS

# Palette : dégradé cyan -> bleu, éléments blancs.
TOP_LEFT = (45, 212, 240)
BOTTOM_RIGHT = (47, 80, 232)


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def gradient(size):
    """Dégradé diagonal, calculé en petit puis agrandi (rapide et lisse)."""
    n = 256
    small = Image.new("RGB", (n, n))
    px = small.load()
    for y in range(n):
        for x in range(n):
            t = (x + y) / (2 * (n - 1))
            px[x, y] = lerp(TOP_LEFT, BOTTOM_RIGHT, t)
    return small.resize((size, size), Image.BICUBIC)


def squircle_mask(size, margin, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [margin, margin, size - margin, size - margin], radius=radius, fill=255
    )
    return mask


def star(draw, cx, cy, r, fill, inner=0.24):
    """Étincelle à 4 branches."""
    pts = []
    for i in range(8):
        ang = math.radians(i * 45 - 90)
        rad = r if i % 2 == 0 else r * inner
        pts.append((cx + rad * math.cos(ang), cy + rad * math.sin(ang)))
    draw.polygon(pts, fill=fill)


def main():
    # --- Fond : carré arrondi style macOS (marge de ~10 %) + ombre douce ---
    margin = round(W * 0.098)
    radius = round(W * 0.225)
    mask = squircle_mask(W, margin, radius)

    shadow = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    shadow.putalpha(mask.filter(ImageFilter.GaussianBlur(W * 0.012)))
    shadow = ImageChops.offset(shadow, 0, round(W * 0.012))
    shadow_black = Image.new("RGBA", (W, W), (0, 0, 0, 90))
    shadow_black.putalpha(ImageChops.multiply(shadow.getchannel("A"), Image.new("L", (W, W), 90)))

    img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    img = Image.alpha_composite(img, shadow_black)

    bg = gradient(W).convert("RGBA")
    bg.putalpha(mask)
    img = Image.alpha_composite(img, bg)

    # Reflet léger en haut, qui s'estompe vers le milieu (pas de bord net).
    g = Image.linear_gradient("L").transpose(Image.FLIP_TOP_BOTTOM)  # 255 en haut -> 0 en bas
    g = g.resize((W, W // 2)).point(lambda v: v * 55 // 255)
    gloss_alpha = Image.new("L", (W, W), 0)
    gloss_alpha.paste(g, (0, 0))
    gloss = Image.new("RGBA", (W, W), (255, 255, 255, 0))
    gloss.putalpha(ImageChops.multiply(gloss_alpha, mask))
    img = Image.alpha_composite(img, gloss)

    # --- Jauge circulaire : piste claire + arc "utilisé" blanc ---
    layer = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    cx = cy = W // 2
    ring_r = round(W * 0.245)
    ring_w = round(W * 0.07)
    box = [cx - ring_r, cy - ring_r, cx + ring_r, cy + ring_r]

    d.arc(box, 0, 360, fill=(255, 255, 255, 70), width=ring_w)

    start, end = -90, 150  # depuis midi, sens horaire (angles PIL : 0 = 3 h)
    d.arc(box, start, end, fill=(255, 255, 255, 255), width=ring_w)
    for ang in (start, end):  # extrémités arrondies
        ex = cx + (ring_r - ring_w / 2) * math.cos(math.radians(ang))
        ey = cy + (ring_r - ring_w / 2) * math.sin(math.radians(ang))
        d.ellipse([ex - ring_w / 2, ey - ring_w / 2, ex + ring_w / 2, ey + ring_w / 2],
                  fill=(255, 255, 255, 255))

    # --- Étincelles "propre" ---
    star(d, cx, cy, W * 0.15, (255, 255, 255, 255))
    star(d, cx + W * 0.215, cy - W * 0.215, W * 0.062, (255, 255, 255, 255))
    star(d, cx - W * 0.225, cy + W * 0.2, W * 0.04, (255, 255, 255, 230))

    # Légère ombre portée sous les éléments blancs.
    sh = layer.copy()
    sh_alpha = sh.getchannel("A").filter(ImageFilter.GaussianBlur(W * 0.008))
    sh_alpha = sh_alpha.point(lambda v: v * 70 // 255)
    sh = Image.new("RGBA", (W, W), (10, 30, 120, 0))
    sh.putalpha(sh_alpha)
    sh = ImageChops.offset(sh, 0, round(W * 0.008))
    img = Image.alpha_composite(img, sh)
    img = Image.alpha_composite(img, layer)

    final = img.resize((SIZE, SIZE), Image.LANCZOS)
    final.save(OUT / "icon.png", optimize=True)
    final.resize((64, 64), Image.LANCZOS).save(OUT / "icon-64.png", optimize=True)
    final.save(OUT / "CleanSpace.icns")
    print("OK :", ", ".join(sorted(p.name for p in OUT.glob("icon*.png"))), "CleanSpace.icns")


if __name__ == "__main__":
    main()
