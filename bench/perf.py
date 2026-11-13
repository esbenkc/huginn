#!/usr/bin/env python3
"""Performance numbers for one Huginn binary, by phase.

Two parts, both without other browsers and without the page corpus:

  load         `huginn render` of each URL, N cold runs. Prints the time per
               phase and the peak memory. Pages are loaded from the network
               unless you pass file paths or local URLs, so `fetch` and `wait`
               are noisy; `cpu` (everything that is not waiting for the
               network) is the number to compare between two binaries.

  interaction  Synthetic pages driven through `huginn open --commands`:
               scrolling, hover, typing, a script that changes the DOM, and
               resizing. Prints how long one update takes (median, 95th
               percentile, maximum) per phase, from the spans in
               crates/huginn-shell/src/perf.rs.

Usage:
  python3 bench/perf.py --binary target/bench-release/huginn --label mychange
  python3 bench/perf.py --only interaction
  python3 bench/perf.py --only load --runs 7 https://example.com/ page.html

Writes bench/results/perf-<date>-<label>.json and .md. Build the binary with
`cargo build --profile bench-release -p huginn-shell`. Compare two binaries on
the same machine, one after the other, with nothing else running.

This does not replace bench.py: it has no fidelity check and no other browser.
"""

import argparse
import datetime
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
PHASES = ["fetch", "parse", "script", "style", "layout", "paint", "encode"]


def default_urls():
    return [p["url"] for p in json.loads((HERE / "corpus.json").read_text())]


def run_render_measured(binary, target, out_png, env):
    """One cold `huginn render`. Returns (report dict or None, peak RSS in MB).
    The peak is the kernel's own figure for this child (wait4)."""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(r)
            os.dup2(w, 1)
            devnull = os.open(os.devnull, os.O_WRONLY)
            os.dup2(devnull, 2)
            os.execve(binary, [binary, "render", target, "--out", out_png], env)
        finally:
            os._exit(127)
    os.close(w)
    chunks = []
    with os.fdopen(r, "rb") as f:
        chunks.append(f.read())
    _, status, ru = os.wait4(pid, 0)
    # ru_maxrss: kilobytes on Linux, bytes on macOS.
    rss_mb = ru.ru_maxrss / (2**20 if sys.platform == "darwin" else 2**10)
    report = None
    for line in b"".join(chunks).decode("utf-8", "replace").splitlines():
        if line.startswith("{"):
            try:
                report = json.loads(line)
            except json.JSONDecodeError:
                pass
    ok = os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0
    return (report if ok else None), rss_mb


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def bench_load(binary, targets, runs, env, tmp):
    pages = {}
    for target in targets:
        reports, rss = [], []
        for _ in range(runs):
            rep, mb = run_render_measured(binary, target, str(tmp / "load.png"), env)
            if rep is None:
                break
            reports.append(rep)
            rss.append(mb)
        if len(reports) < runs:
            print(f"  FAILED  {target}", flush=True)
            pages[target] = {"failed": True}
            continue
        row = {p: med([r.get(f"{p}_ms") for r in reports]) for p in PHASES}
        row["total"] = med([r["total_ms"] for r in reports])
        row["total_min"] = min(r["total_ms"] for r in reports)
        row["total_max"] = max(r["total_ms"] for r in reports)
        row["script_layout"] = med([r.get("script_layout_ms") for r in reports])
        row["script_layouts"] = med([r.get("script_layouts") for r in reports])
        row["script_timers"] = med([r.get("script_timers_ms") for r in reports])
        waits = [r.get("script_wait_ms") for r in reports]
        row["wait"] = med(waits)
        # Work that does not depend on the network: every phase but fetch,
        # minus the time scripts waited (unknown where the system does not say).
        cpu = []
        for r in reports:
            c = sum(r.get(f"{p}_ms") or 0 for p in PHASES if p != "fetch")
            cpu.append(c - (r.get("script_wait_ms") or 0))
        row["cpu"] = med(cpu)
        row["rss_mb"] = med(rss)
        row["nodes"] = reports[0].get("nodes")
        row["js_errors"] = med([r.get("js_errors") for r in reports])
        pages[target] = row
        print(
            f"  {short(target):34s} cpu {row['cpu']:7.0f}  total {row['total']:7.0f}  rss {row['rss_mb']:5.0f} MB",
            flush=True,
        )
    return pages


def short(target):
    return target.replace("https://", "").replace("http://", "")[:34]


# ---- interaction ------------------------------------------------------------

def page_long(n):
    paras = "\n".join(
        f"<p>Paragraph {i}. The quick brown fox jumps over the lazy dog, "
        f"<a href='#p{i}'>link {i}</a> and <b>bold</b> text to wrap over lines.</p>"
        for i in range(n)
    )
    return f"<!doctype html><title>long</title><style>body{{font:16px sans-serif;margin:20px}}p{{margin:8px 0}}</style>{paras}"


def page_hover(n):
    cells = "".join(f"<a class=c href='#{i}'>item {i}</a>" for i in range(n))
    return (
        "<!doctype html><title>hover</title><style>body{margin:0;font:14px sans-serif}"
        ".g{display:grid;grid-template-columns:repeat(10,1fr)}"
        ".c{display:block;padding:6px;height:20px;color:#036}.c:hover{background:#fd6;color:#000}</style>"
        f"<div class=g>{cells}</div>"
    )


def page_type(n):
    rows = "".join(f"<li>row {i} with some text</li>" for i in range(n))
    return (
        "<!doctype html><title>type</title><style>body{margin:0;font:14px sans-serif}"
        "input{display:block;width:600px;height:40px;font-size:20px;margin:0}</style>"
        f"<input id=i><ul>{rows}</ul>"
    )


def page_dom(n):
    rows = "".join(f"<li class=r>row {i} <span>with a span</span></li>" for i in range(n))
    return (
        "<!doctype html><title>dom</title><style>body{margin:0;font:14px sans-serif}"
        "button{display:block;width:300px;height:40px;margin:0}"
        ".on .r span{color:#c00}</style>"
        "<button id=cls>toggle class</button><button id=add>append</button>"
        f"<ul id=l>{rows}</ul>"
        "<script>"
        "document.getElementById('cls').onclick=()=>document.body.classList.toggle('on');"
        "document.getElementById('add').onclick=()=>{const li=document.createElement('li');"
        "li.textContent='new row';document.getElementById('l').prepend(li)};"
        "</script>"
    )


def scenarios(tmp):
    """(name, what one step is, page, commands). Page coordinates are window
    CSS px; the page starts 80 px down (tab strip and toolbar)."""
    def write(name, html):
        p = tmp / f"{name}.html"
        p.write_text(html)
        return str(p)

    out = []
    step = "scroll 120; frame; "
    out.append(("scroll", "scroll 120 px and draw a frame, 5000 paragraphs",
                write("long", page_long(5000)),
                "wait; frame; perf reset; " + step * 100 + "perf"))
    step = "".join(f"move {40 + 97 * (i % 12)} {100 + 32 * (i % 17)}; wait; frame; " for i in range(60))
    out.append(("hover", "move the pointer to another :hover target, 3000 links",
                write("hover", page_hover(3000)),
                "wait; frame; perf reset; " + step + "perf"))
    step = "".join("type x; wait; frame; " for _ in range(50))
    out.append(("type", "type one character in a text field, 3000 list items",
                write("type", page_type(3000)),
                "wait; click 100 100; wait; frame; perf reset; " + step + "perf"))
    step = "click 100 100; wait; frame; " * 30
    out.append(("class", "a script toggles a class on <body>, 3000 list items",
                write("dom", page_dom(3000)),
                "wait; frame; perf reset; " + step + "perf"))
    step = "click 100 140; wait; frame; " * 30
    out.append(("append", "a script adds one node, 3000 list items",
                write("dom", page_dom(3000)),
                "wait; frame; perf reset; " + step + "perf"))
    step = "".join(f"resize {1000 + 20 * (i % 10)} 700; wait; frame; " for i in range(20))
    out.append(("resize", "resize the window by 20 px, 5000 paragraphs",
                write("long", page_long(5000)),
                "wait; frame; perf reset; " + step + "perf"))
    return out


def bench_interaction(binary, env, tmp):
    results = {}
    for name, what, page, commands in scenarios(tmp):
        try:
            proc = subprocess.run(
                [binary, "open", page, "--width", "1200", "--height", "800", "--commands", commands],
                capture_output=True, env=env, timeout=600,
            )
        except subprocess.TimeoutExpired:
            print(f"  TIMEOUT {name}", flush=True)
            results[name] = {"what": what, "failed": True}
            continue
        perf = None
        for line in proc.stdout.decode("utf-8", "replace").splitlines():
            if line.startswith('{"perf"'):
                perf = json.loads(line)["perf"]
        if proc.returncode != 0 or perf is None:
            print(f"  FAILED  {name}: {proc.stderr.decode('utf-8', 'replace')[-300:]}", flush=True)
            results[name] = {"what": what, "failed": True}
            continue
        results[name] = {"what": what, "spans": perf}
        parts = "  ".join(
            f"{k} {v['p50_ms']:.1f}/{v['p95_ms']:.1f}" for k, v in perf.items() if k != "present"
        )
        print(f"  {name:8s} p50/p95 ms: {parts}", flush=True)
    return results


# ---- output -----------------------------------------------------------------

def fmt(v, digits=0):
    return "-" if v is None else f"{v:.{digits}f}"


def markdown(report):
    L = [f"# Huginn performance: {report['label']}", ""]
    m = report["meta"]
    L += [
        f"- date: {m['date']}",
        f"- commit: {m['commit']}",
        f"- binary: `{m['binary']}`",
        f"- machine: {m['machine']}",
        f"- runs per page: {m['runs']}",
        "",
    ]
    if report.get("load"):
        L += [
            "## Load",
            "",
            "Median of the runs, in ms. `cpu` is all work that does not wait for the",
            "network (every phase but `fetch`, minus the time scripts waited). `total` is",
            "process start to PNG written, with its lowest and highest run. Pages come",
            "from the live network unless the target is a file, so only `cpu`, the",
            "phases other than `fetch`, and memory are comparable between runs.",
            "",
            "| page | cpu | parse | script | of which timers | script waited | script layout (passes) | style | layout | paint | total (min-max) | peak RSS MB | nodes |",
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
        ]
        for target, r in report["load"].items():
            if r.get("failed"):
                L.append(f"| {short(target)} | failed | | | | | | | | | | | |")
                continue
            L.append(
                f"| {short(target)} | {fmt(r['cpu'])} | {fmt(r['parse'])} | {fmt(r['script'])} | {fmt(r['script_timers'])} "
                f"| {fmt(r['wait'])} | {fmt(r['script_layout'])} ({fmt(r['script_layouts'])}) | {fmt(r['style'])} "
                f"| {fmt(r['layout'])} | {fmt(r['paint'])} | {fmt(r['total'])} ({fmt(r['total_min'])}-{fmt(r['total_max'])}) "
                f"| {fmt(r['rss_mb'])} | {r['nodes']} |"
            )
        ok = [r for r in report["load"].values() if not r.get("failed")]
        if ok:
            gm = statistics.geometric_mean
            L += ["", f"Geometric mean over {len(ok)} pages: cpu {gm([max(r['cpu'], 0.1) for r in ok]):.0f} ms, "
                      f"peak RSS {gm([r['rss_mb'] for r in ok]):.0f} MB."]
        L.append("")
    if report.get("interaction"):
        L += [
            "## Interaction",
            "",
            "Synthetic pages, 1200x800 window at scale 1, no window system. Each cell is",
            "median / 95th percentile / maximum in ms for one step. `update` is one whole",
            "page update on the engine thread (restyle, new images, layout); `style` and",
            "`layout` are its parts. `raster` is one 256 px strip; `compose` is one frame.",
            "",
            "| scenario | one step is | update | style | layout | raster | compose |",
            "|---|---|---|---|---|---|---|",
        ]
        for name, r in report["interaction"].items():
            if r.get("failed"):
                L.append(f"| {name} | {r['what']} | failed | | | | |")
                continue
            def cell(k):
                s = r["spans"].get(k)
                return "-" if not s else f"{s['p50_ms']:.1f} / {s['p95_ms']:.1f} / {s['max_ms']:.1f} (n={s['count']})"
            L.append(f"| {name} | {r['what']} | {cell('update')} | {cell('style')} | {cell('layout')} | {cell('raster')} | {cell('compose')} |")
        L.append("")
    return "\n".join(L)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("targets", nargs="*", help="URLs or files for the load part (default: the URLs in corpus.json)")
    ap.add_argument("--binary", default=str(HERE.parent / "target" / "bench-release" / "huginn"))
    ap.add_argument("--label", default="run")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--only", choices=["load", "interaction"])
    ap.add_argument("--v8-flags", help="set HUGINN_V8_FLAGS for the binary")
    args = ap.parse_args()

    binary = str(Path(args.binary).resolve())
    if not os.access(binary, os.X_OK):
        sys.exit(f"no binary at {binary}; build it with: cargo build --profile bench-release -p huginn-shell")
    env = dict(os.environ)
    if args.v8_flags:
        env["HUGINN_V8_FLAGS"] = args.v8_flags
    commit = subprocess.run(["git", "-C", str(HERE), "rev-parse", "--short", "HEAD"],
                            capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(HERE), "status", "--porcelain", "--untracked-files=no"],
                           capture_output=True, text=True).stdout.strip()
    date = datetime.date.today().isoformat()
    report = {
        "label": args.label,
        "meta": {
            "date": date,
            "commit": commit + (" (with uncommitted changes)" if dirty else ""),
            "binary": os.path.relpath(binary, HERE.parent),
            "machine": f"{platform.system()} {platform.machine()}, {os.cpu_count()} CPUs",
            "runs": args.runs,
            "v8_flags": args.v8_flags,
        },
    }
    with tempfile.TemporaryDirectory(prefix="huginn-perf-") as tmp:
        tmp = Path(tmp)
        if args.only != "interaction":
            print("load", flush=True)
            report["load"] = bench_load(binary, args.targets or default_urls(), args.runs, env, tmp)
        if args.only != "load":
            print("interaction", flush=True)
            report["interaction"] = bench_interaction(binary, env, tmp)
    out = HERE / "results"
    out.mkdir(exist_ok=True)
    stem = out / f"perf-{date}-{args.label}"
    stem.with_suffix(".json").write_text(json.dumps(report, indent=1))
    stem.with_suffix(".md").write_text(markdown(report))
    print(f"wrote {os.path.relpath(stem.with_suffix('.md'), HERE.parent)}")


if __name__ == "__main__":
    main()
