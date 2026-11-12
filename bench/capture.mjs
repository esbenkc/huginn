// Snapshot one live page (and every subresource Chrome fetched for it) into
// corpus/<name>/ so serve.py can replay it from 127.0.0.1 without the network.
//
// Usage: node capture.mjs <name>          (reads url/port from corpus.json)
//        node capture.mjs --all
//
// Output per page:
//   corpus/<name>/manifest.json   key ("host/path?query") -> {file, type, status, location}
//   corpus/<name>/files/*         response bodies, absolute URLs rewritten to the local server
//   corpus/<name>/capture.png     Chrome's screenshot of the live page at capture time (1280x800)
import puppeteer from 'puppeteer-core';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const ROOT = path.dirname(new URL(import.meta.url).pathname);
const corpus = JSON.parse(fs.readFileSync(path.join(ROOT, 'corpus.json'), 'utf8'));

const TEXT_TYPES = /(text\/|javascript|json|xml|svg|ecmascript|manifest)/i;
const UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36';

function keyOf(u) {
  const url = new URL(u);
  return url.host + url.pathname + url.search;
}

function extFor(type, u) {
  const t = (type || '').split(';')[0].trim();
  const map = {
    'text/html': '.html', 'text/css': '.css', 'application/javascript': '.js', 'text/javascript': '.js',
    'application/json': '.json', 'image/png': '.png', 'image/jpeg': '.jpg', 'image/gif': '.gif',
    'image/webp': '.webp', 'image/avif': '.avif', 'image/svg+xml': '.svg', 'font/woff2': '.woff2',
    'font/woff': '.woff', 'font/ttf': '.ttf', 'image/x-icon': '.ico', 'image/vnd.microsoft.icon': '.ico',
  };
  if (map[t]) return map[t];
  const m = new URL(u).pathname.match(/\.([a-z0-9]{1,5})$/i);
  return m ? '.' + m[1].toLowerCase() : '.bin';
}

function escapeRe(s) { return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); }

// Build a rewriter that maps every captured host onto the local server.
// Main host -> http://127.0.0.1:PORT, other hosts -> http://127.0.0.1:PORT/__h/<host>.
function makeRewriter(hosts, mainHost, port) {
  const origin = `http://127.0.0.1:${port}`;
  const rules = [];
  // Longest hosts first so "a.b.com" wins over "b.com".
  for (const h of [...hosts].sort((a, b) => b.length - a.length)) {
    const target = h === mainHost ? origin : `${origin}/__h/${h}`;
    const eh = escapeRe(h);
    rules.push([new RegExp(`(?:https?:)?//${eh}(?![\\w.-])`, 'g'), target]);
    rules.push([new RegExp(`(?:https?:)?\\\\/\\\\/${eh}(?![\\w.-])`, 'g'), target.replace(/\//g, '\\/')]);
  }
  return (text, isHtml) => {
    for (const [re, rep] of rules) text = text.replace(re, rep);
    if (isHtml) {
      // Rewriting changes bytes, so subresource integrity hashes would fail. CSP could block the local origin.
      text = text.replace(/\s(integrity)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi, '');
      text = text.replace(/<meta[^>]+http-equiv\s*=\s*["']?content-security-policy["']?[^>]*>/gi, '');
    }
    return text;
  };
}

async function capture(entry) {
  const outDir = path.join(ROOT, 'corpus', entry.name);
  fs.rmSync(outDir, { recursive: true, force: true });
  fs.mkdirSync(path.join(outDir, 'files'), { recursive: true });

  const userDataDir = fs.mkdtempSync('/tmp/huginn-bench-capture-');
  const browser = await puppeteer.launch({
    executablePath: CHROME,
    headless: true,
    userDataDir,
    args: ['--window-size=1280,800', '--hide-scrollbars', '--no-first-run', '--no-default-browser-check'],
    defaultViewport: { width: 1280, height: 800 },
  });
  const page = await browser.newPage();
  await page.setUserAgent(UA);

  const records = new Map(); // key -> {url, status, type, location, body}
  const pending = [];
  page.on('response', (resp) => {
    const u = resp.url();
    if (!/^https?:/.test(u)) return;
    const status = resp.status();
    const headers = resp.headers();
    const rec = { url: u, status, type: headers['content-type'] || '', location: headers['location'] || null, body: null };
    if (status >= 300 && status < 400) { records.set(keyOf(u), rec); return; }
    pending.push(resp.buffer().then((b) => { rec.body = b; records.set(keyOf(u), rec); }).catch(() => {}));
  });

  const t0 = Date.now();
  await page.goto(entry.url, { waitUntil: 'networkidle2', timeout: 90000 });
  // Trigger lazy-loaded content, then return to the top.
  await page.evaluate(async () => {
    const step = 600;
    for (let y = 0; y < Math.min(document.body.scrollHeight, 30000); y += step) {
      window.scrollTo(0, y);
      await new Promise((r) => setTimeout(r, 80));
    }
    window.scrollTo(0, 0);
  });
  await page.waitForNetworkIdle({ idleTime: 1000, timeout: 20000 }).catch(() => {});
  await new Promise((r) => setTimeout(r, 1000));
  await page.screenshot({ path: path.join(outDir, 'capture.png') });
  await Promise.all(pending);
  const finalUrl = page.url();
  await browser.close();
  fs.rmSync(userDataDir, { recursive: true, force: true });

  const mainHost = new URL(finalUrl).host;
  const hosts = new Set([mainHost, new URL(entry.url).host]);
  for (const r of records.values()) hosts.add(new URL(r.url).host);
  const rewrite = makeRewriter(hosts, mainHost, entry.port);
  const finalKey = keyOf(finalUrl);

  const manifest = { name: entry.name, source_url: entry.url, final_url: finalUrl, main_host: mainHost, port: entry.port,
    local_url: `http://127.0.0.1:${entry.port}` + new URL(finalUrl).pathname + new URL(finalUrl).search,
    captured_at: new Date().toISOString(), entries: {} };
  let bytes = 0;
  for (const [key, r] of records) {
    if (r.status >= 300 && r.status < 400) {
      if (key === finalKey) continue;
      const loc = r.location ? rewrite(new URL(r.location, r.url).href, false) : null;
      manifest.entries[key] = { status: r.status, location: loc };
      continue;
    }
    if (!r.body) continue;
    let body = r.body;
    if (TEXT_TYPES.test(r.type)) {
      const isHtml = /html/i.test(r.type);
      body = Buffer.from(rewrite(body.toString('utf8'), isHtml), 'utf8');
    }
    const file = crypto.createHash('sha1').update(key).digest('hex').slice(0, 16) + extFor(r.type, r.url);
    fs.writeFileSync(path.join(outDir, 'files', file), body);
    bytes += body.length;
    manifest.entries[key] = { status: r.status === 200 ? 200 : r.status, type: r.type, file };
  }
  manifest.resources = Object.keys(manifest.entries).length;
  manifest.bytes = bytes;
  fs.writeFileSync(path.join(outDir, 'manifest.json'), JSON.stringify(manifest, null, 1));
  console.log(`${entry.name}: ${manifest.resources} resources, ${(bytes / 1e6).toFixed(2)} MB, ${hosts.size} hosts, ${Date.now() - t0} ms -> ${manifest.local_url}`);
}

const args = process.argv.slice(2);
const targets = args.includes('--all') ? corpus : corpus.filter((e) => args.includes(e.name));
if (!targets.length) { console.error('usage: node capture.mjs <name...> | --all'); process.exit(2); }
for (const e of targets) {
  try { await capture(e); } catch (err) { console.error(`${e.name}: FAILED ${err.message}`); }
}
