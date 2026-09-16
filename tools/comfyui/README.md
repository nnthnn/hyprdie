# Remote ComfyUI on the Windows desktop

Supporting infrastructure for the `logos/` work: a diffusion runtime on a Windows
box with a CUDA GPU, driven over the LAN from another machine.

Addresses below are placeholders. `<render-host>` is the machine running ComfyUI;
`<serving-host>` is whichever machine serves the installer.

## Why not Ollama?

Because it cannot do this on that machine. Evidence, not assumption:

```console
$ curl -s -X POST http://<render-host>:11434/api/pull -d '{"model":"x/flux2-klein","stream":false}'
{"error":"this model requires MLX support, but the MLX runtime is not available"}
```

The model metadata is `"model_format":"safetensors"`, `"capabilities":["image"]`
with `"requires":"0.14.0"` — so the gate is neither the weights nor the GPU.
Ollama's image pipeline is **MLX-only** (Apple's framework). Every
`x/flux2-klein` tag (`latest`, `4b`, `4b-fp8`, `4b-bf16`, `9b-fp8`) hits the
same wall. No model you pull will change that, so Ollama stays as-is for text.

## Install (on the desktop, in PowerShell)

The script is served over HTTP from another machine, so it can be updated
without re-pasting anything long.

```powershell
curl.exe -L http://<serving-host>:8321/setup-comfyui.ps1 -o $env:TEMP\setup-comfyui.ps1
powershell -ExecutionPolicy Bypass -File $env:TEMP\setup-comfyui.ps1
```

Run the firewall step in an **Administrator** PowerShell; the script prints the
exact command if you are not elevated.

### Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `-InstallRoot` | `%USERPROFILE%\ComfyUI` | Needs ~30–45 GB free |
| `-ModelSet` | `zimage` | `zimage`, `sdxl`, or `none` |
| `-Variant` | `balanced` | zimage only: `quality` / `balanced` / `compact` |
| `-Port` | `8188` | API port |
| `-AllowFrom` | `''` | Restrict the API to one host; empty allows the whole LAN |
| `-SkipFirewall` | off | Skip the inbound rule |

### What gets downloaded

| Piece | Size |
| --- | --- |
| `ComfyUI_windows_portable_nvidia.7z` (v0.35.0) | 1.91 GB |
| Z-Image Turbo `bf16` diffusion model | 12.31 GB |
| Z-Image Turbo `qwen_3_4b_fp8_mixed` text encoder | 5.63 GB |
| `ae.safetensors` VAE | 0.34 GB |
| distill patch LoRA | 0.16 GB |

`-Variant compact` swaps in the `int8_convrot` diffusion model (6.20 GB) for a
much smaller VRAM footprint. `-ModelSet sdxl` pulls the single-file SDXL
checkpoint (6.9 GB) instead, which is the lowest-risk smoke test.

## Start it

```powershell
& "$env:USERPROFILE\ComfyUI\ComfyUI_windows_portable\run-listen.bat"
```

It binds `0.0.0.0:8188`. The firewall rule restricts inbound to the host you name
in `-AllowFrom`.

## Drive it from the other machine

```sh
python3 tools/comfyui/comfy_client.py --info                 # confirm reachability + dump node types
python3 tools/comfyui/comfy_client.py --run wf.json --out out/
```

`--info` also writes `tools/comfyui/object_info.json`, which is what the
workflow JSON gets built against — querying the live install beats guessing at
node names for the ComfyUI version you actually have.

## Status: working

Verified against the reference install: RTX 3090, ComfyUI 0.35.0, Python 3.13,
listening on `0.0.0.0:8188`. **~7.9 s per 1024x1024 image** with the model already
resident (model load on the first run is the slow part).

The graph is in `workflows/z-image-turbo.json`, flattened out of the official
`image_z_image_turbo` template that ships with the install. Four things about it
are not guessable and are the reason it exists as a file:

| Detail | Value | Why it matters |
| --- | --- | --- |
| Text encoder loader | `CLIPLoader` type **`lumina2`** | There is no `z_image` option in the 28-value type enum; Z-Image's Qwen3 encoder loads as `lumina2`. |
| Model sampling | `ModelSamplingAuraFlow`, `shift=3` | Z-Image needs this; omitting it degrades output. |
| Sampler | `res_multistep` / `simple` | Not the usual `euler`/`normal`. |
| Steps / CFG | `8` / `1.0` | It is a distilled turbo model; more steps do not help. |

The negative conditioning is `ConditioningZeroOut` applied to the positive
prompt, and no LoRA is required — the official template ignores the
`z_image_turbo_distill_patch_lora` entirely.

### Use it

```sh
python3 tools/comfyui/comfy_client.py --info                      # health + VRAM + node inventory
python3 tools/comfyui/comfy_client.py --queue                     # queue depth

python3 tools/comfyui/comfy_client.py \
  --run tools/comfyui/workflows/z-image-turbo.json \
  --out out/ \
  --seed 42 \
  --set "__PROMPT__=your prompt here"
```

`--seed -1` randomises. Passing `--set` with a numeric value into a numeric
input keeps it an int, so `steps` and `cfg` are overridable too.

### Verdict on using it for the logo

It generates, but the output is the wrong material for a logo: soft edges,
inconsistent geometry between seeds, and no vector output. The assets in
`assets/` are the shippable ones. This pipeline is useful for moodboards and for
the app's background image, not for the mark itself.

## Undo

Nothing is installed system-wide (bar one firewall rule). Delete
`%USERPROFILE%\ComfyUI`, and remove the rule with:

```powershell
Get-NetFirewallRule -DisplayName 'ComfyUI 8188 (LAN)' | Remove-NetFirewallRule
```
