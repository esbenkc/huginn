#!/usr/bin/env python3
"""Replay the captured corpus from 127.0.0.1, one port per page.

Usage: python3 serve.py [--log-misses] [page ...]

Each page lives on its own port (see corpus.json) so root-relative URLs like
"/static/app.js" resolve against the page's own snapshot. Resources from other
hosts were rewritten at capture time to /__h/<host>/<path>.

This is a static server: it only serves bytes captured by capture.mjs, with the
recorded Content-Type. Unknown paths get 404. No compression, no caching
headers, HTTP/1.1 keep-alive.
"""
import json
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent
LOG_MISSES = "--log-misses" in sys.argv


def load_site(name):
    d = ROOT / "corpus" / name
    manifest = json.loads((d / "manifest.json").read_text())
    return d, manifest


def make_handler(site_dir, manifest):
    entries = manifest["entries"]
    main_host = manifest["main_host"]
    # Fallback index without the query string, for cache-busting params that differ per load.
    by_path = {}
    for key, e in entries.items():
        by_path.setdefault(key.split("?", 1)[0], e)

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "huginn-bench"
        sys_version = ""

        def log_message(self, fmt, *args):
            pass

        def _lookup(self):
            p = self.path
            if p.startswith("/__h/"):
                key = p[len("/__h/"):]
            else:
                key = main_host + p
            key = key.split("#", 1)[0]
            e = entries.get(key)
            if e is None:
                e = by_path.get(key.split("?", 1)[0])
            return key, e

        def _send(self, head_only):
            key, e = self._lookup()
            if e is None:
                if LOG_MISSES:
                    sys.stderr.write(f"[{manifest['name']}] 404 {key}\n")
                body = b"not in snapshot"
                self.send_response(404)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if not head_only:
                    self.wfile.write(body)
                return
            if e.get("location"):
                self.send_response(e["status"])
                self.send_header("Location", e["location"])
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body = (site_dir / "files" / e["file"]).read_bytes()
            self.send_response(e.get("status", 200))
            self.send_header("Content-Type", e.get("type") or "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            if not head_only:
                self.wfile.write(body)

        def do_GET(self):
            self._send(False)

        def do_HEAD(self):
            self._send(True)

        def do_POST(self):
            # Analytics beacons and similar. Accept and drop.
            length = int(self.headers.get("Content-Length") or 0)
            if length:
                self.rfile.read(length)
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_OPTIONS(self):
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, HEAD, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "*")
            self.send_header("Content-Length", "0")
            self.end_headers()

    return Handler


# A DNS server that answers NXDOMAIN to every query, at once. Ladybird is pointed here
# (--dns-server=127.0.0.1 --dns-port=DNS_PORT) so a lookup of a host outside the snapshot
# fails as fast as Chrome's --host-resolver-rules "MAP * ~NOTFOUND". Without it, Ladybird
# waits 5 s for a DNS timeout on every page with an uncaptured third-party request.
DNS_PORT = 8153


def nxdomain(query):
    if len(query) < 12:
        return None
    qdcount = int.from_bytes(query[4:6], "big")
    end = 12
    for _ in range(qdcount):  # skip each question: labels, then QTYPE and QCLASS
        while end < len(query) and query[end] != 0:
            end += 1 + query[end]
        end += 5
    if end > len(query):
        return None
    rd = query[2] & 0x01
    flags = bytes([0x80 | (query[2] & 0x78) | rd, 0x80 | 3])  # QR, opcode, RD; RA, RCODE=NXDOMAIN
    return query[0:2] + flags + query[4:6] + b"\0\0\0\0\0\0" + query[12:end]


def serve_dns():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", DNS_PORT))
    while True:
        query, addr = sock.recvfrom(4096)
        reply = nxdomain(query)
        if reply:
            sock.sendto(reply, addr)


def main():
    corpus = json.loads((ROOT / "corpus.json").read_text())
    wanted = [a for a in sys.argv[1:] if not a.startswith("--")]
    servers = []
    for entry in corpus:
        if wanted and entry["name"] not in wanted:
            continue
        if not (ROOT / "corpus" / entry["name"] / "manifest.json").exists():
            print(f"skip {entry['name']}: not captured", file=sys.stderr)
            continue
        site_dir, manifest = load_site(entry["name"])
        httpd = ThreadingHTTPServer(("127.0.0.1", entry["port"]), make_handler(site_dir, manifest))
        httpd.daemon_threads = True
        servers.append(httpd)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        print(f"{entry['name']:12s} {manifest['local_url']}", flush=True)
    threading.Thread(target=serve_dns, daemon=True).start()
    print("serving", len(servers), f"pages; NXDOMAIN DNS on udp 127.0.0.1:{DNS_PORT}", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
