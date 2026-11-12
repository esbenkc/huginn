#!/usr/bin/env python3
"""Huginn page-load benchmark: Huginn vs Ladybird, with Chrome as the visual reference.

For each corpus page and each browser, run N cold headless loads that end in a
1280x800 screenshot. Record wall time, peak memory of the whole process tree,
and SSIM of the screenshot against Chrome's screenshot of the same page.

    python3 bench.py --browsers chrome,ladybird[,huginn] [--runs 5] [--pages a,b] [--label baseline]

See README.md for the method and WIN.md for the win formula.
"""
import os
import threading
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
_VENV_PY = ROOT / ".venv" / "bin" / "python"

try:
    import numpy as np
    from PIL import Image, ImageDraw, ImageFont
    from skimage.metrics import structural_similarity
except ImportError:
    # System python3 has no numpy/skimage; re-run under the bench venv (once).
    if _VENV_PY.exists() and not os.environ.get("HUGINN_BENCH_IN_VENV"):
        os.environ["HUGINN_BENCH_IN_VENV"] = "1"
        os.execv(str(_VENV_PY), [str(_VENV_PY), sys.argv[0], *sys.argv[1:]])
    sys.exit("missing numpy/pillow/scikit-image: python3 -m venv .venv && .venv/bin/pip install numpy pillow scikit-image")

import argparse
import datetime as dt
import json
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import tempfile
import time

from procmem import ProcessTreeSampler, tree_pids
from serve import DNS_PORT  # serve.py answers NXDOMAIN to every DNS query on this UDP port

W, H = 1280, 800
CHROME = os.environ.get("CHROME") or str(
    ROOT / "browsers/chrome-headless-shell/mac_arm-157.0.8103.51/chrome-headless-shell-mac-arm64/chrome-headless-shell")
LADYBIRD = os.environ.get("LADYBIRD") or str(
    Path.home() / "src/ladybird/Build/release/bin/Ladybird.app/Contents/MacOS/Ladybird")
HUGINN = os.environ.get("HUGINN")
ALL_BROWSERS = ["chrome", "ladybird", "huginn"]


# ---------------------------------------------------------------- browsers

def chrome_cmd(url, png, profile):
    return [CHROME, "--headless", f"--user-data-dir={profile}", "--no-first-run", "--no-default-browser-check",
            "--use-mock-keychain", "--disable-background-networking", "--disable-component-update",
            "--disable-sync", "--disable-extensions", "--hide-scrollbars", "--force-device-scale-factor=1",
            f"--window-size={W},{H}", "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1",
            f"--screenshot={png}", url]


def ladybird_cmd(url, png, profile):
    # --headless=screenshot with --screenshot-on-load: the screenshot is taken when the
    # load event fires (see ladybird-screenshot-on-load.patch), not after a fixed delay.
    # A fresh --profile-path also forces a new browser process (--force-new-process is
    # deprecated upstream and cannot be combined with --profile-path).
    return [LADYBIRD, "--headless=screenshot", "--screenshot-on-load", f"--screenshot-path={png}",
            f"--window-width={W}", f"--window-height={H}",
            f"--profile-path={profile}", "--disable-http-disk-cache",
            "--dns-server=127.0.0.1", f"--dns-port={DNS_PORT}", url]


def huginn_cmd(url, png, profile):
    return [HUGINN, "render", url, "--out", png, "--width", str(W), "--height", str(H)]


COMMANDS = {"chrome": chrome_cmd, "ladybird": ladybird_cmd, "huginn": huginn_cmd}

# Extra environment per browser. HUGINN_NET_LOOPBACK_ONLY makes huginn fail every
# non-loopback host at once, which mirrors Chrome's --host-resolver-rules and
# Ladybird's --dns-server above: all three browsers are offline except 127.0.0.1.
ENV = {"huginn": {"HUGINN_NET_LOOPBACK_ONLY": "1"}}


def browser_env(name):
    return {**os.environ, **ENV.get(name, {})}


def binary(name):
    return {"chrome": CHROME, "ladybird": LADYBIRD, "huginn": HUGINN}[name]


def browser_version(name):
    try:
        if name == "chrome":
            return subprocess.run([CHROME, "--version"], capture_output=True, text=True, timeout=30).stdout.strip()
        if name == "ladybird":
            src = Path(LADYBIRD).parents[6] if "Build" in LADYBIRD else None  # <src>/Build/release/bin/Ladybird.app/Contents/MacOS
            if src and (src / ".git").exists():
                h = subprocess.run(["git", "-C", str(src), "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
                dirty = subprocess.run(["git", "-C", str(src), "status", "--porcelain", "-uno"], capture_output=True, text=True).stdout.strip()
                return f"ladybird {h}" + (" + local patch (ladybird-screenshot-on-load.patch)" if dirty else "")
            return "ladybird (unknown commit)"
        if name == "huginn":
            out = subprocess.run([HUGINN, "--version"], capture_output=True, text=True, timeout=30)
            return (out.stdout or out.stderr).strip().splitlines()[0] if (out.stdout or out.stderr).strip() else "huginn"
    except Exception as e:  # noqa: BLE001
        return f"{name} (version unknown: {e})"
    return name


# ---------------------------------------------------------------- one run

def run_once(browser, url, png, timeout):
    """One cold load. Returns a dict with wall_ms, peak memory, exit status, stdout JSON (huginn)."""
    profile = tempfile.mkdtemp(prefix=f"huginn-bench-{browser}-")
    Path(png).unlink(missing_ok=True)
    cmd = COMMANDS[browser](url, png, profile)
    t0 = time.perf_counter()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
                            env=browser_env(browser))
    sampler = ProcessTreeSampler(proc.pid, pgid=proc.pid, interval=0.02).start()
    # Read the pipes in threads and reap the root with wait4, so we get its exact peak RSS from
    # the kernel (ru_maxrss). The 20 ms sampler misses processes that live only a few ms.
    bufs = {}
    readers = [threading.Thread(target=lambda n=n, f=f: bufs.__setitem__(n, f.read()), daemon=True)
               for n, f in (("out", proc.stdout), ("err", proc.stderr))]
    for r in readers:
        r.start()
    timed_out = False
    while True:
        pid, status, ru = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        if time.perf_counter() - t0 > timeout:
            timed_out = True
            _kill_tree(proc.pid)
            pid, status, ru = os.wait4(proc.pid, 0)
            break
        time.sleep(0.002)
    proc.returncode = os.waitstatus_to_exitcode(status)
    root_maxrss = ru.ru_maxrss  # bytes on macOS
    for r in readers:
        r.join(timeout=5)
    out, err = bufs.get("out", b""), bufs.get("err", b"")
    wall_ms = 1000 * (time.perf_counter() - t0)
    sampler.stop()
    _kill_tree(proc.pid)  # helpers that outlive the root process
    shutil.rmtree(profile, ignore_errors=True)

    result = {
        "wall_ms": round(wall_ms, 1),
        # Peak RSS = max(sampled sum over the process tree, kernel peak of the root process).
        # For multi-process browsers the sum dominates; for a short single process the kernel value does.
        "peak_rss_mb": round(max(sampler.peak_rss, root_maxrss) / 2**20, 1),
        "sampled_tree_rss_mb": round(sampler.peak_rss / 2**20, 1),
        "root_maxrss_mb": round(root_maxrss / 2**20, 1),
        "peak_footprint_mb": round(sampler.peak_footprint / 2**20, 1),
        "sum_proc_peak_footprint_mb": round(sum(sampler.lifetime_max_footprint.values()) / 2**20, 1),
        "max_procs": sampler.max_procs,
        "samples": sampler.samples,
        "process_names": sorted(set(sampler.names.values())),
        "exit_code": proc.returncode,
        "timed_out": timed_out,
        "screenshot": Path(png).exists(),
    }
    result["ok"] = (not timed_out) and proc.returncode == 0 and result["screenshot"]
    if browser == "huginn":
        for line in out.decode(errors="replace").splitlines():
            line = line.strip()
            if line.startswith("{"):
                try:
                    result["huginn_json"] = json.loads(line)
                except json.JSONDecodeError:
                    pass
    if not result["ok"]:
        result["stderr_tail"] = err.decode(errors="replace")[-600:]
    return result


def _kill_tree(pid):
    pids = tree_pids(pid, pgid=pid)
    for p in pids:
        try:
            os.kill(p, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    try:
        os.killpg(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass


# ---------------------------------------------------------------- images

def load_viewport(png):
    """Open a screenshot as 1280x800 RGB: crop anything bigger (Ladybird may save the full page), pad with white."""
    if not png or not Path(png).exists():
        return None
    try:
        im = Image.open(png).convert("RGB")
    except Exception:  # noqa: BLE001
        return None
    if im.size != (W, H):
        canvas = Image.new("RGB", (W, H), "white")
        canvas.paste(im.crop((0, 0, min(W, im.width), min(H, im.height))), (0, 0))
        im = canvas
    return im


def ssim(a, b):
    if a is None or b is None:
        return 0.0
    ga = np.asarray(a.convert("L"), dtype=np.float64)
    gb = np.asarray(b.convert("L"), dtype=np.float64)
    return float(structural_similarity(ga, gb, data_range=255.0))


# ---------------------------------------------------------------- server

def port_open(port):
    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", port)) == 0


def dns_answers():
    """True if serve.py's NXDOMAIN DNS answers on DNS_PORT (an older serve.py has no DNS)."""
    query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.settimeout(0.5)
        try:
            s.sendto(query, ("127.0.0.1", DNS_PORT))
            return s.recv(512)[:2] == b"\x12\x34"
        except OSError:
            return False


def ensure_server(pages):
    ports = [p["port"] for p in pages]
    if all(port_open(p) for p in ports):
        if not dns_answers():
            sys.exit(f"corpus ports are open but nothing answers DNS on udp {DNS_PORT}: restart serve.py")
        return None
    proc = subprocess.Popen([sys.executable, str(ROOT / "serve.py")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.time() + 15
    while time.time() < deadline:
        if all(port_open(p) for p in ports) and dns_answers():
            return proc
        time.sleep(0.1)
    proc.kill()
    sys.exit("serve.py did not come up")


# ---------------------------------------------------------------- main

def median(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 1) if xs else None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--browsers", default="chrome,ladybird,huginn")
    ap.add_argument("--pages", default="", help="comma list of corpus page names (default: all)")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--timeout", type=float, default=60.0, help="seconds per run before the tree is killed")
    ap.add_argument("--label", default="bench", help="results/<label>-<date>.json, results/<label>.md")
    ap.add_argument("--no-warmup", action="store_true")
    ap.add_argument("--compare", default=None,
                    help="results JSON to take Ladybird numbers from when ladybird is not in --browsers "
                         "(default: newest results JSON with Ladybird)")
    ap.add_argument("--update-ref", action="store_true",
                    help="also overwrite the shared developer reference in results/ref/chrome/")
    args = ap.parse_args()

    browsers = [b.strip() for b in args.browsers.split(",") if b.strip()]
    for b in browsers:
        if b not in ALL_BROWSERS:
            sys.exit(f"unknown browser {b}")
    if "huginn" in browsers and not (HUGINN and os.access(HUGINN, os.X_OK)):
        print("huginn: HUGINN env var not set or not executable; skipping huginn", file=sys.stderr)
        browsers.remove("huginn")
    for b in list(browsers):
        if not os.access(binary(b), os.X_OK):
            print(f"{b}: binary not found at {binary(b)}; skipping", file=sys.stderr)
            browsers.remove(b)
    if not browsers:
        sys.exit("no runnable browsers")

    corpus = json.loads((ROOT / "corpus.json").read_text())
    if args.pages:
        wanted = args.pages.split(",")
        corpus = [p for p in corpus if p["name"] in wanted]
    pages = []
    for p in corpus:
        mf = ROOT / "corpus" / p["name"] / "manifest.json"
        if not mf.exists():
            print(f"{p['name']}: not captured, skipping (node capture.mjs {p['name']})", file=sys.stderr)
            continue
        p = dict(p, local_url=json.loads(mf.read_text())["local_url"])
        pages.append(p)

    server = ensure_server(pages)
    date = dt.datetime.now().strftime("%Y-%m-%d")
    stamp = dt.datetime.now().strftime("%Y-%m-%dT%H%M%S")
    shots = ROOT / "results" / "shots" / f"{args.label}-{stamp}"
    # Each run's Chrome reference is saved with that run's shots. The shared developer reference in
    # results/ref/chrome/ (used by fidelity.py) is only written with --update-ref, so it stays stable.
    refdir = ROOT / "results" / "ref" / "chrome"
    run_refdir = shots / "chrome-ref"
    run_refdir.mkdir(parents=True, exist_ok=True)

    report = {
        "label": args.label, "date": date, "started": stamp, "runs_per_page": args.runs,
        "viewport": [W, H],
        "machine": {"platform": platform.platform(), "cpu_count": os.cpu_count(), "python": platform.python_version()},
        "versions": {b: browser_version(b) for b in browsers},
        "binaries": {b: binary(b) for b in browsers},
        "pages": {},
    }

    try:
        if not args.no_warmup:
            # First launch of a binary pays macOS code-signature checks and cold dyld caches. Not measured.
            for b in browsers:
                d = shots / "_warmup"
                d.mkdir(parents=True, exist_ok=True)
                r = run_once(b, pages[0]["local_url"], str(d / f"{b}.png"), args.timeout)
                print(f"warmup {b}: {r['wall_ms']:.0f} ms ok={r['ok']}", flush=True)

        for page in pages:
            name = page["name"]
            entry = {"url": page["source_url"] if "source_url" in page else page["url"], "local_url": page["local_url"],
                     "kind": page.get("kind"), "browsers": {b: {"runs": []} for b in browsers}}
            for i in range(args.runs):
                # Rotate the browser order each run so drift (thermals, caches) does not favour one browser.
                order = browsers[i % len(browsers):] + browsers[:i % len(browsers)]
                for b in order:
                    d = shots / b
                    d.mkdir(parents=True, exist_ok=True)
                    png = str(d / f"{name}-{i}.png")
                    r = run_once(b, page["local_url"], png, args.timeout)
                    r["png"] = str(Path(png).relative_to(ROOT)) if r["screenshot"] else None
                    entry["browsers"][b]["runs"].append(r)
                    print(f"{name:12s} {b:9s} run {i}: {r['wall_ms']:8.0f} ms  rss {r['peak_rss_mb']:7.1f} MB  "
                          f"footprint {r['peak_footprint_mb']:7.1f} MB  procs {r['max_procs']:2d}  ok={r['ok']}", flush=True)

            # Chrome reference: the Chrome run most similar to the other Chrome runs (medoid).
            ref = None
            if "chrome" in browsers:
                imgs = [load_viewport(ROOT / r["png"]) if r["png"] else None for r in entry["browsers"]["chrome"]["runs"]]
                valid = [k for k, im in enumerate(imgs) if im is not None]
                if valid:
                    best = max(valid, key=lambda k: sum(ssim(imgs[k], imgs[j]) for j in valid if j != k))
                    ref = imgs[best]
                    ref.save(run_refdir / f"{name}.png")
                    if args.update_ref:
                        refdir.mkdir(parents=True, exist_ok=True)
                        ref.save(refdir / f"{name}.png")
                    entry["chrome_ref"] = str((run_refdir / f"{name}.png").relative_to(ROOT))
                    entry["browsers"]["chrome"]["runs"][best]["is_reference"] = True
            elif (refdir / f"{name}.png").exists():
                ref = load_viewport(refdir / f"{name}.png")
                entry["chrome_ref"] = str((refdir / f"{name}.png").relative_to(ROOT)) + " (from an earlier run)"

            for b in browsers:
                runs = entry["browsers"][b]["runs"]
                for r in runs:
                    r["ssim"] = round(ssim(load_viewport(ROOT / r["png"]) if r["png"] else None, ref), 4) if ref is not None else None
                ok = [r for r in runs if r["ok"]]
                agg = entry["browsers"][b]
                agg["ok_runs"] = len(ok)
                # Failed runs still count for time and memory: a timeout costs the full timeout.
                agg["median_wall_ms"] = median([r["wall_ms"] for r in runs])
                agg["median_peak_rss_mb"] = median([r["peak_rss_mb"] for r in runs])
                agg["median_peak_footprint_mb"] = median([r["peak_footprint_mb"] for r in runs])
                # Chrome's own SSIM (its run-to-run noise floor) leaves out the reference run, which is 1.0 by definition.
                scored = [r["ssim"] for r in runs if not r.get("is_reference")] or [r["ssim"] for r in runs]
                agg["median_ssim"] = round(statistics.median(scored), 4) if ref is not None and scored else None
                agg["max_procs"] = max(r["max_procs"] for r in runs)
                # Screenshot used in the contact sheet: Chrome's reference run, else the run with the median SSIM.
                if runs:
                    refs = [r for r in runs if r.get("is_reference")]
                    pick = refs[0] if refs else sorted(runs, key=lambda r: (r["ssim"] or 0))[len(runs) // 2]
                    agg["sheet_png"] = pick["png"]
            report["pages"][name] = entry
    finally:
        if server:
            server.terminate()

    out_dir = ROOT / "results"
    json_path = out_dir / f"{args.label}-{date}.json"
    json_path.write_text(json.dumps(report, indent=1))
    verdict = evaluate(report, args.compare)
    report["verdict"] = verdict
    json_path.write_text(json.dumps(report, indent=1))
    sheet = out_dir / f"{args.label}-contact.png"
    contact_sheet(report, browsers, sheet)
    md = out_dir / f"{args.label}.md"
    md.write_text(markdown(report, browsers, json_path, sheet))
    print(f"\nwrote {json_path.relative_to(ROOT)}, {md.relative_to(ROOT)}, {sheet.relative_to(ROOT)}")
    if verdict:
        print(verdict["summary"])


# ---------------------------------------------------------------- win formula (WIN.md)

FIDELITY_SLACK = 0.02


def latest_ladybird_results():
    """Newest results JSON (by start time) that has Ladybird numbers."""
    best = None
    for p in (ROOT / "results").glob("*.json"):
        try:
            r = json.loads(p.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if "ladybird" in r.get("versions", {}) and (best is None or r["started"] > best[0]):
            best = (r["started"], p)
    return best[1] if best else None


def evaluate(report, compare=None):
    """Per page: huginn vs Ladybird on memory, time, fidelity. See WIN.md.

    If this run has huginn but not Ladybird, Ladybird's numbers come from `compare`
    (a results JSON), or the newest results JSON with Ladybird in it.
    """
    pages = report["pages"]
    if not pages:
        return None
    sample = next(iter(pages.values()))["browsers"]
    if "huginn" not in sample:
        return None
    ladybird_from = "this run"
    if "ladybird" in sample:
        lb = {name: e["browsers"]["ladybird"] for name, e in pages.items()}
    else:
        path = Path(compare) if compare else latest_ladybird_results()
        if not path or not path.exists():
            return None
        old = json.loads(path.read_text())
        lb = {name: e["browsers"]["ladybird"] for name, e in old["pages"].items() if "ladybird" in e["browsers"]}
        ladybird_from = str(path.relative_to(ROOT) if path.is_relative_to(ROOT) else path)
    rows = {}
    for name, e in pages.items():
        if name not in lb:
            continue
        k, l = e["browsers"]["huginn"], lb[name]
        k_failed = k["ok_runs"] == 0
        rows[name] = {
            "memory": (not k_failed) and k["median_peak_rss_mb"] < l["median_peak_rss_mb"],
            "time": (not k_failed) and k["median_wall_ms"] < l["median_wall_ms"],
            "fidelity": (k["median_ssim"] or 0) >= (l["median_ssim"] or 0) - FIDELITY_SLACK,
        }
    if not rows:
        return None
    n = len(rows)
    share = {m: sum(r[m] for r in rows.values()) / n for m in ("memory", "time", "fidelity")}
    win = share["memory"] >= 0.8 and share["time"] >= 0.8 and share["fidelity"] >= 0.5
    summary = (f"huginn vs Ladybird over {n} pages: memory {share['memory']:.0%} of pages, time {share['time']:.0%}, "
               f"fidelity {share['fidelity']:.0%} -> {'WIN' if win else 'no win yet'} "
               f"(needs memory>=80%, time>=80%, fidelity>=50%). Ladybird numbers from {ladybird_from}.")
    return {"per_page": rows, "share": share, "win": win, "summary": summary, "ladybird_from": ladybird_from}


# ---------------------------------------------------------------- outputs

def _font(size):
    try:
        return ImageFont.load_default(size=size)
    except TypeError:
        return ImageFont.load_default()


def contact_sheet(report, browsers, path):
    tw, th = 400, 250
    pad, head, label_h = 12, 40, 34
    pages = list(report["pages"].items())
    cols = len(browsers)
    width = pad + cols * (tw + pad) + 150
    height = head + len(pages) * (th + label_h + pad) + pad
    sheet = Image.new("RGB", (width, height), (245, 245, 245))
    d = ImageDraw.Draw(sheet)
    f_big, f_small = _font(20), _font(14)
    for c, b in enumerate(browsers):
        d.text((150 + pad + c * (tw + pad), 10), b, fill="black", font=f_big)
    for r, (name, e) in enumerate(pages):
        y = head + r * (th + label_h + pad)
        d.text((pad, y + th // 2 - 10), name, fill="black", font=f_big)
        for c, b in enumerate(browsers):
            x = 150 + pad + c * (tw + pad)
            agg = e["browsers"][b]
            im = load_viewport(ROOT / agg["sheet_png"]) if agg.get("sheet_png") else None
            if im is None:
                d.rectangle([x, y, x + tw, y + th], fill=(220, 200, 200))
                d.text((x + 10, y + 10), "no screenshot", fill="black", font=f_small)
            else:
                sheet.paste(im.resize((tw, th), Image.LANCZOS), (x, y))
                d.rectangle([x - 1, y - 1, x + tw, y + th], outline=(150, 150, 150))
            ssim_s = f"{agg['median_ssim']:.3f}" if agg.get("median_ssim") is not None else "-"
            d.text((x, y + th + 4), f"{agg['median_wall_ms']:.0f} ms  {agg['median_peak_rss_mb']:.0f} MB RSS  SSIM {ssim_s}",
                   fill="black", font=f_small)
    sheet.save(path)


def markdown(report, browsers, json_path, sheet):
    L = []
    L.append(f"# Benchmark: {report['label']} ({report['date']})\n")
    L.append(f"Runs per page: {report['runs_per_page']}. Viewport {W}x{H}. Values are medians over runs.")
    L.append("Wall = launch to exit, ms. RSS = peak of the summed resident size of the whole process tree, MB.")
    L.append("Footprint = same, summed phys_footprint (Activity Monitor's \"Memory\"). SSIM = vs the Chrome reference screenshot.\n")
    L.append("Versions: " + "; ".join(f"{b}: {v}" for b, v in report["versions"].items()) + "\n")
    hdr = ["page"]
    for b in browsers:
        hdr += [f"{b} wall", f"{b} RSS", f"{b} footprint", f"{b} SSIM"]
        if b != "chrome":
            hdr.append(f"{b} ok")
    L.append("| " + " | ".join(hdr) + " |")
    L.append("|" + "---|" * len(hdr))
    for name, e in report["pages"].items():
        row = [name]
        for b in browsers:
            a = e["browsers"][b]
            row += [f"{a['median_wall_ms']:.0f}", f"{a['median_peak_rss_mb']:.0f}", f"{a['median_peak_footprint_mb']:.0f}",
                    "-" if a["median_ssim"] is None else f"{a['median_ssim']:.3f}"]
            if b != "chrome":
                row.append(f"{a['ok_runs']}/{len(a['runs'])}")
        L.append("| " + " | ".join(row) + " |")
    # Geometric-mean summary row.
    L.append("")
    for b in browsers:
        walls = [e["browsers"][b]["median_wall_ms"] for e in report["pages"].values()]
        rss = [e["browsers"][b]["median_peak_rss_mb"] for e in report["pages"].values()]
        ss = [e["browsers"][b]["median_ssim"] for e in report["pages"].values() if e["browsers"][b]["median_ssim"] is not None]
        gm = lambda xs: float(np.exp(np.mean(np.log(np.maximum(xs, 1e-9))))) if xs else float("nan")
        L.append(f"- {b}: geomean wall {gm(walls):.0f} ms, geomean peak RSS {gm(rss):.0f} MB, mean SSIM {np.mean(ss) if ss else float('nan'):.3f}")
    if report.get("verdict"):
        v = report["verdict"]
        L.append(f"\n## Verdict\n\n{v['summary']}\n")
        L.append("| page | memory | time | fidelity |\n|---|---|---|---|")
        for name, r in v["per_page"].items():
            L.append(f"| {name} | {'win' if r['memory'] else 'loss'} | {'win' if r['time'] else 'loss'} | {'win' if r['fidelity'] else 'loss'} |")
    L.append(f"\nRaw data: `{json_path.relative_to(ROOT)}`. Contact sheet: `{sheet.relative_to(ROOT)}`.")
    return "\n".join(L) + "\n"


if __name__ == "__main__":
    main()
