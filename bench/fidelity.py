#!/usr/bin/env python3
"""Quick fidelity check for Huginn developers (not the benchmark).

Renders corpus pages with a huginn binary at background QoS and prints SSIM vs the
Chrome reference in results/ref/chrome/. Single run, no timing claims; use bench.py for
real measurements.

  .venv/bin/python fidelity.py --huginn <path/to/huginn> [--pages hackernews,mdn] [--out DIR]
"""
import argparse, json, pathlib, subprocess, sys, time

HERE = pathlib.Path(__file__).resolve().parent
sys.argv, _argv = [sys.argv[0]], sys.argv[1:]
import bench  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--huginn", required=True)
ap.add_argument("--pages", default="")
ap.add_argument("--out", default="/tmp/huginn-fidelity")
a = ap.parse_args(_argv)

pages = json.load(open(HERE / "corpus.json"))
if a.pages:
    want = set(a.pages.split(","))
    pages = [p for p in pages if p["name"] in want]
bench.ensure_server(json.load(open(HERE / "corpus.json")))
out = pathlib.Path(a.out); out.mkdir(parents=True, exist_ok=True)
rows = []
for p in pages:
    mf = HERE / "corpus" / p["name"] / "manifest.json"
    url = json.loads(mf.read_text())["local_url"] if mf.exists() else f"http://127.0.0.1:{p['port']}/"
    png = out / f"{p['name']}.png"
    png.unlink(missing_ok=True)
    t = time.time()
    # bench.browser_env: offline except loopback, like the benchmark runs.
    r = subprocess.run(["taskpolicy", "-b", "nice", "-n", "19", a.huginn, "render", url, "--out", str(png),
                        "--width", "1280", "--height", "800"], capture_output=True, text=True, timeout=300,
                       env=bench.browser_env("huginn"))
    wall = time.time() - t
    s = bench.ssim(bench.load_viewport(str(png)), bench.load_viewport(str(HERE / "results/ref/chrome" / f"{p['name']}.png")))
    rows.append((p["name"], r.returncode, s, wall))
    print(f"{p['name']:<12} exit={r.returncode} ssim={s:.3f} wall(bg QoS)={wall:.2f}s {r.stdout.strip()[:160]}", flush=True)
print(f"mean ssim {sum(x[2] for x in rows)/max(1,len(rows)):.3f} over {len(rows)} pages")
