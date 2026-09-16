#!/usr/bin/env python3
"""Trace a flat raster image into an SVG (potrace, via the pure-Python `potracer`).

This is the bridge from "generated concept" to "shippable asset": diffusion
output is raster and anti-aliased, the logos/ set is crisp vector.

Setup (no sudo needed):
    python3 -m venv /tmp/opencode/tracer
    /tmp/opencode/tracer/bin/pip install potracer pillow numpy

Usage:
    trace.py --in concept.png --out wordmark.svg
    trace.py --in concept.png --out wordmark.svg --invert       # dark-on-light
    trace.py --in concept.png --out wordmark.svg --threshold 100 --fill '#acb5c7'

Notes:
  * Threshold first: these models emit gradients and near-miss greys that no
    tracer handles well. The image should be genuinely two-tone.
  * Raise --turdsize to drop speckle, lower --opttolerance for more faithful
    (and heavier) curves. You will still almost certainly want to hand-clean the
    result in Inkscape before shipping it.
"""
from __future__ import annotations

import argparse
import sys

try:
    import numpy as np
    import potrace
    from PIL import Image
except ImportError as e:  # pragma: no cover
    sys.exit(f"missing dependency: {e}\nsee the setup notes at the top of this file")


def curve_to_path(curve) -> str:
    p = curve.start_point
    d = [f"M {p.x:.2f} {p.y:.2f}"]
    for seg in curve.segments:
        if seg.is_corner:
            d.append(f"L {seg.c.x:.2f} {seg.c.y:.2f}")
            d.append(f"L {seg.end_point.x:.2f} {seg.end_point.y:.2f}")
        else:
            d.append(
                "C {:.2f} {:.2f} {:.2f} {:.2f} {:.2f} {:.2f}".format(
                    seg.c1.x, seg.c1.y, seg.c2.x, seg.c2.y, seg.end_point.x, seg.end_point.y
                )
            )
    d.append("Z")
    return " ".join(d)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--in", dest="src", required=True, help="input raster (png/jpg/...)")
    ap.add_argument("--out", dest="dst", required=True, help="output .svg")
    ap.add_argument("--threshold", type=int, default=128, help="0-255 luminance cut (default 128)")
    ap.add_argument("--invert", action="store_true", help="treat dark pixels as the shape")
    ap.add_argument("--fill", default="#ffffff", help="shape colour (default #ffffff)")
    ap.add_argument("--background", default=None, help="optional background fill, e.g. '#1b1818'")
    ap.add_argument("--turdsize", type=int, default=8, help="drop shapes smaller than N px (default 8)")
    ap.add_argument("--alphamax", type=float, default=1.0, help="corner threshold, 0=all corners (default 1.0)")
    ap.add_argument("--opttolerance", type=float, default=0.2, help="curve optimisation, 0=faithful (default 0.2)")
    ap.add_argument("--scale", type=float, default=1.0, help="scale coordinates (default 1.0)")
    args = ap.parse_args()

    img = Image.open(args.src).convert("L")
    arr = np.array(img)
    mask = arr > args.threshold
    if args.invert:
        mask = ~mask

    on = int(mask.sum())
    if on == 0:
        sys.exit(f"nothing to trace: threshold {args.threshold} selected 0 pixels")
    if on == mask.size:
        print("warning: every pixel selected; try --invert", file=sys.stderr)

    # potracer treats LOW values as foreground, so feed it the complement of the
    # shape mask. Verified empirically on a real sample: passing the mask as-is
    # traced the background (100% coverage), the complement traced the glyphs
    # (22.5%) against a 21.4% source.
    path = potrace.Bitmap(~mask).trace(
        turdsize=args.turdsize, alphamax=args.alphamax, opttolerance=args.opttolerance
    )
    curves = list(path)
    if not curves:
        sys.exit("trace produced no curves")

    w, h = img.size
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" role="img">'
    ]
    if args.background:
        parts.append(f'  <rect width="{w}" height="{h}" fill="{args.background}"/>')
    # All curves must live in ONE <path>: fill-rule is evaluated per path element,
    # so separate <path> elements cannot punch the counters out of letters like
    # R, D and P.
    d = " ".join(curve_to_path(c) for c in curves)
    parts.append(f'  <g transform="scale({args.scale})">')
    parts.append(f'    <path d="{d}" fill="{args.fill}" fill-rule="evenodd"/>')
    parts.append("  </g>")
    parts.append("</svg>")

    with open(args.dst, "w") as fh:
        fh.write("\n".join(parts) + "\n")

    nodes = sum(1 for c in curves for _ in c.segments)
    print(f"{args.src} -> {args.dst}")
    print(f"  {w}x{h}, {len(curves)} curves, {nodes} segments, {on/(w*h):.1%} of pixels selected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
