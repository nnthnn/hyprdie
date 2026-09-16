# Melting / tombstone concepts

Generated with the remote ComfyUI pipeline (`tools/comfyui/`), not drawn by hand.
This is the ideation branch of the logo work: deliberately weirder, at the cost
of the crispness the hand-authored marks in `logos/` have.

## Provenance

| | |
| --- | --- |
| Model | Z-Image Turbo (6B, Apache-2.0) |
| Workflow | `tools/comfyui/workflows/z-image-turbo.json` |
| Hardware | RTX 3090, 24 GB, ~8 s per image |
| Prompts | see `PROMPTS.txt` |

`concepts.png` is a contact sheet; the original 1024x1024 / 1344x768 renders are
regenerable from the prompts and seeds, so they are not committed.

## What is here

| File | Source seed | Notes |
| --- | --- | --- |
| `UC-s7.svg`, `UC-s11.svg`, `UC-s12.svg` | 7, 11, 12 | Dripping `HYPRDIE` wordmark, white on near-black |
| `E-skull-s2.svg` | 2 | Melting skull |
| `D-melt-pixel-s2.svg` | 2 | Melting 2x2 tile grid — closest to `mark-tiles-fall.svg` |
| `A-tomb-s2.svg` | 2 | Tombstone with melting skull |

## Two findings worth keeping

**1. Uppercase text is reliable, lowercase is not.** Every one of 8 uppercase
`HYPRDIE` seeds spelled correctly. In the lowercase batch, 3 of 4 produced
garbled output (`hyrdie`, `hypdie`, `hypde`) and one duplicated the word. If you
want generated type, use uppercase.

**2. Single-tone tracing loses detail, by construction.** `trace.py` thresholds
to one mask (`--threshold`, default 128), so anything that is not genuinely
two-tone gets flattened. `A-tomb-s2` had a grey stone texture and black interior
lines; the trace kept only the white silhouette and dropped both. Tracing
multi-tone art needs one mask per tone, which is not implemented.

All four SVGs still carry tracer wobble on the edges and would want a cleanup
pass in Inkscape before shipping.

## The pipeline

```sh
# 1. generate (--seed -1 to randomise)
python3 tools/comfyui/comfy_client.py \
  --run tools/comfyui/workflows/z-image-turbo.json \
  --out out/crazy --seed 7 --size 1344x768 \
  --set "__PROMPT__=<prompt>" --set "z-image-turbo=UC-s7"

# 2. pick something

# 3. vectorise
python3 tools/comfyui/trace.py --in out/crazy/UC-s7_00001_.png --out logos/melting/UC-s7.svg
```

`trace.py` needs `potracer`, `pillow` and `numpy` — see its docstring for the
venv setup.
