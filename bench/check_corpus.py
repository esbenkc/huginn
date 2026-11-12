#!/usr/bin/env python3
"""Check that each snapshot renders in Chrome from the local server like the live page.

For every corpus page, chrome-headless-shell takes two 1280x800 screenshots:
  corpus/<name>/local.png  the snapshot from 127.0.0.1, all other hosts blocked
  corpus/<name>/live.png   the live URL from the internet, right now
It prints SSIM(local, live), the number of requests the snapshot could not answer
(404s from serve.py), and writes corpus/check.png (live | local side by side).
Low SSIM means the snapshot misses something, or the live page changed since capture
(news, ads, carousels, dates). Look at check.png and the 404 list to tell which.

    python3 check_corpus.py [--reuse-live] [page ...]

--reuse-live: do not fetch the live page again; compare against the existing live.png.
"""
import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import bench  # re-execs under .venv if needed
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent


def shoot(url, png, offline):
    profile = tempfile.mkdtemp(prefix="huginn-bench-check-")
    cmd = bench.chrome_cmd(url, str(png), profile)
    if not offline:
        cmd = [a for a in cmd if not a.startswith("--host-resolver-rules")]
    subprocess.run(cmd, capture_output=True, timeout=120)
    shutil.rmtree(profile, ignore_errors=True)
    return bench.load_viewport(png)


def main():
    corpus = json.loads((ROOT / "corpus.json").read_text())
    reuse_live = "--reuse-live" in sys.argv
    wanted = [a for a in sys.argv[1:] if not a.startswith("--")]
    pages = [p for p in corpus if not wanted or p["name"] in wanted]
    if any(bench.port_open(p["port"]) for p in pages):
        sys.exit("corpus ports are already in use; stop the running serve.py so misses can be logged")
    miss_log = tempfile.NamedTemporaryFile(prefix="huginn-bench-misses-", suffix=".log", delete=False)
    server = subprocess.Popen([sys.executable, str(ROOT / "serve.py"), "--log-misses"] + [p["name"] for p in pages],
                              stdout=subprocess.DEVNULL, stderr=miss_log)
    deadline = time.time() + 15
    while not all(bench.port_open(p["port"]) for p in pages):
        if time.time() > deadline:
            server.kill()
            sys.exit("serve.py did not come up")
        time.sleep(0.1)
    rows = []
    try:
        for p in pages:
            d = ROOT / "corpus" / p["name"]
            mf = json.loads((d / "manifest.json").read_text())
            local = shoot(mf["local_url"], d / "local.png", offline=True)
            if reuse_live and (d / "live.png").exists():
                live = bench.load_viewport(d / "live.png")
            else:
                live = shoot(p["url"], d / "live.png", offline=False)
            time.sleep(0.2)  # let the server flush late 404 lines
            tag = f"[{p['name']}] 404 "
            misses = sorted({ln[len(tag):].strip() for ln in Path(miss_log.name).read_text().splitlines()
                             if ln.startswith(tag)})
            s = bench.ssim(local, live)
            rows.append((p["name"], live, local, s))
            print(f"{p['name']:12s} resources {mf['resources']:4d}  {mf['bytes'] / 1e6:6.2f} MB  "
                  f"SSIM(local vs live) {s:.3f}  404s {len(misses)}", flush=True)
            for m in misses:
                print(f"    404 {m[:160]}", flush=True)
    finally:
        server.terminate()
        Path(miss_log.name).unlink(missing_ok=True)

    tw, th, pad = 480, 300, 10
    sheet = Image.new("RGB", (150 + 2 * (tw + pad), 30 + len(rows) * (th + pad)), (245, 245, 245))
    dr = ImageDraw.Draw(sheet)
    font = bench._font(18)
    dr.text((150, 5), "live (internet)", fill="black", font=font)
    dr.text((150 + tw + pad, 5), "local snapshot", fill="black", font=font)
    for i, (name, live, local, s) in enumerate(rows):
        y = 30 + i * (th + pad)
        dr.text((5, y + th // 2 - 20), f"{name}\nSSIM {s:.3f}", fill="black", font=font)
        for j, im in enumerate((live, local)):
            if im is not None:
                sheet.paste(im.resize((tw, th), Image.LANCZOS), (150 + j * (tw + pad), y))
    sheet.save(ROOT / "corpus" / "check.png")
    print("wrote corpus/check.png")


if __name__ == "__main__":
    main()
