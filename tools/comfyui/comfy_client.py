#!/usr/bin/env python3
"""Talk to a remote ComfyUI instance over its HTTP API.

  comfy_client.py --info                      reachability + node inventory
  comfy_client.py --run wf.json --out out/    queue a workflow, save the images
  comfy_client.py --queue                     what's currently running

`--info` dumps object_info.json next to this script. That file is the source of
truth for which node types and inputs the installed ComfyUI actually has, so
build workflows against it rather than against memory.
"""
from __future__ import annotations

import argparse
import json
import os
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
# Point COMFY_HOST at the machine running ComfyUI, e.g. COMFY_HOST=192.0.2.10:8188
DEFAULT_HOST = os.environ.get("COMFY_HOST", "127.0.0.1:8188")


def _get(host: str, path: str, timeout: float = 30.0):
    url = f"http://{host}{path}"
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.loads(r.read().decode())


def _post(host: str, path: str, payload: dict, timeout: float = 60.0):
    url = f"http://{host}{path}"
    body = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())


def cmd_info(host: str) -> int:
    try:
        stats = _get(host, "/system_stats")
    except (urllib.error.URLError, OSError) as e:
        print(f"cannot reach http://{host} — {e}", file=sys.stderr)
        print("is run-listen.bat running, and is the firewall rule in place?", file=sys.stderr)
        return 1

    for d in stats.get("devices", []):
        free = d.get("vram_free", 0) / 1e9
        total = d.get("vram_total", 0) / 1e9
        print(f"device  {d.get('name')}  {free:.1f}/{total:.1f} GB VRAM free")
    print(f"comfy   {stats.get('system', {}).get('comfyui_version')}")
    print(f"python  {stats.get('system', {}).get('python_version', '').split()[0]}")
    for k in ("argv", "ram_free", "ram_total"):
        if k in stats.get("system", {}):
            print(f"{k:7} {stats['system'][k]}")

    info = _get(host, "/object_info", timeout=60.0)
    out = os.path.join(HERE, "object_info.json")
    with open(out, "w") as fh:
        json.dump(info, fh, indent=1, sort_keys=True)

    interesting = sorted(
        n for n in info
        if any(t in n for t in ("Loader", "Sampler", "CLIPTextEncode", "Latent", "VAEDecode", "SaveImage", "Lora"))
    )
    print(f"\n{len(info)} node types -> {out}")
    print(f"{len(interesting)} relevant to text-to-image:")
    for n in interesting:
        print(f"  {n}")
    return 0


def _load_workflow(path: str, overrides: dict[str, str], seed: int | None,
                   size: tuple[int, int] | None = None) -> dict:
    wf = json.load(open(path))
    wf.pop("_comment", None)

    def coerce(current, value: str):
        """Keep ints as ints so numeric inputs are not sent as strings."""
        if isinstance(current, bool) or not isinstance(current, (int, float)):
            return value
        try:
            return int(value) if isinstance(current, int) else float(value)
        except ValueError:
            return value

    def walk(node: dict):
        for k, v in list(node.items()):
            if isinstance(v, str) and v in overrides:
                node[k] = overrides[v]
            elif isinstance(v, list):
                for i, item in enumerate(v):
                    if isinstance(item, str) and item in overrides:
                        v[i] = overrides[item]
            elif isinstance(v, dict):
                walk(v)

    for node in wf.values():
        if not isinstance(node, dict):
            continue
        walk(node)
        ins = node.get("inputs")
        # Apply a seed to every node that takes one, ComfyUI-style.
        if seed is not None and isinstance(ins, dict) and "seed" in ins:
            ins["seed"] = seed if seed >= 0 else random.randint(0, 2**48)
        # Apply the requested canvas size to the latent node.
        if size is not None and isinstance(ins, dict):
            if "width" in ins:
                ins["width"] = size[0]
            if "height" in ins:
                ins["height"] = size[1]
    return wf


def cmd_run(host: str, workflow: str, out: str, overrides: dict[str, str],
            seed: int | None, size: tuple[int, int] | None, timeout: float) -> int:
    wf = _load_workflow(workflow, overrides, seed, size)
    client_id = str(uuid.uuid4())

    started = _post(host, "/prompt", {"prompt": wf, "client_id": client_id})
    if started.get("node_errors"):
        print("workflow rejected:", file=sys.stderr)
        print(json.dumps(started["node_errors"], indent=2)[:4000], file=sys.stderr)
        return 1
    prompt_id = started.get("prompt_id")
    print(f"queued {prompt_id}")

    deadline = time.time() + timeout
    history = None
    while time.time() < deadline:
        time.sleep(1.5)
        try:
            h = _get(host, f"/history/{prompt_id}")
        except (urllib.error.URLError, OSError):
            continue
        if prompt_id in h:
            history = h[prompt_id]
            break
    if history is None:
        print(f"timed out after {timeout:.0f}s — still running; check the ComfyUI console", file=sys.stderr)
        return 1

    status = history.get("status", {})
    if status.get("status_str") == "error":
        print("execution error:", file=sys.stderr)
        print(json.dumps(status, indent=2)[:4000], file=sys.stderr)
        return 1

    os.makedirs(out, exist_ok=True)
    saved = []
    for node_id, node_out in history.get("outputs", {}).items():
        for img in node_out.get("images", []):
            q = urllib.parse.urlencode({
                "filename": img["filename"],
                "subfolder": img.get("subfolder", ""),
                "type": img.get("type", "output"),
            })
            dest = os.path.join(out, img["filename"])
            with urllib.request.urlopen(f"http://{host}/view?{q}", timeout=120) as r, open(dest, "wb") as fh:
                fh.write(r.read())
            saved.append(dest)
            print(f"saved {dest}")
    if not saved:
        print("no images in history payload", file=sys.stderr)
        return 1
    return 0


def cmd_queue(host: str) -> int:
    q = _get(host, "/queue")
    print(f"running {len(q.get('queue_running', []))}  pending {len(q.get('queue_pending', []))}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default=DEFAULT_HOST, help=f"host:port (default {DEFAULT_HOST})")
    ap.add_argument("--info", action="store_true", help="reachability check + dump object_info.json")
    ap.add_argument("--run", metavar="WORKFLOW.json", help="queue a workflow in ComfyUI API format")
    ap.add_argument("--out", default="out", help="output directory for --run (default: ./out)")
    ap.add_argument("--set", action="append", default=[], metavar="KEY=VALUE",
                    help="replace a string value in the workflow (repeatable)")
    ap.add_argument("--seed", type=int, default=None,
                    help="set every 'seed' input (use -1 to randomise per run)")
    ap.add_argument("--size", metavar="WxH", default=None,
                    help="set the latent canvas size, e.g. 1344x768")
    ap.add_argument("--timeout", type=float, default=600.0, help="seconds to wait (default 600)")
    ap.add_argument("--queue", action="store_true", help="show queue depth")
    args = ap.parse_args()

    overrides = {}
    for pair in args.set:
        if "=" not in pair:
            ap.error(f"--set expects KEY=VALUE, got {pair!r}")
        k, v = pair.split("=", 1)
        overrides[k] = v

    size = None
    if args.size:
        try:
            w, h = args.size.lower().split("x")
            size = (int(w), int(h))
        except ValueError:
            ap.error(f"--size expects WxH, got {args.size!r}")

    if args.info:
        return cmd_info(args.host)
    if args.queue:
        return cmd_queue(args.host)
    if args.run:
        return cmd_run(args.host, args.run, args.out, overrides, args.seed, size, args.timeout)
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
