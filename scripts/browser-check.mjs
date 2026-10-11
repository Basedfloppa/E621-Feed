#!/usr/bin/env node
// Boot the SPA in a real headless Chrome and report whether it came up.
//
// A unit test can assert that `dist/index.html` contains a script tag; it cannot
// catch the failures that actually break this frontend, because all of them are
// decided by the browser at load time:
//
//   * the Content-Security-Policy blocks the inline loader Trunk injects into
//     `index.html` (`script-src` must keep `'unsafe-inline'`), and the SPA never
//     boots at all;
//   * the WASM is served as `text/html` (the SPA fallback swallowing a missing
//     asset) and `instantiateStreaming` refuses it;
//   * a sub-resource fails its Subresource-Integrity hash after a partial deploy;
//   * the app mounts to an empty `<body>` and only a console error says so.
//
// Chrome is driven over the DevTools protocol directly. Neither a browser
// automation library nor any other package is required: Node ships a global
// `WebSocket` (>= 22) and the rest is built in.
//
// If no browser binary can be found the check is skipped with exit code 0, so it
// can be called unconditionally on runners that have no Chrome.
//
// Usage:
//   node scripts/browser-check.mjs --url http://127.0.0.1:8181
//   node scripts/browser-check.mjs --json
//
// Environment: BROWSER_CHECK_URL, BROWSER_CHECK_TIMEOUT, CHROME_PATH.

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';

const opts = parseArgs(process.argv.slice(2));
const url = opts.url ?? process.env.BROWSER_CHECK_URL ?? 'http://127.0.0.1:8181';
const timeoutMs = Number(opts.timeout ?? process.env.BROWSER_CHECK_TIMEOUT ?? 30_000);
const asJson = Boolean(opts.json);

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--json') out.json = true;
    else if (a.startsWith('--')) out[a.slice(2)] = argv[i + 1]?.startsWith('--') ? true : argv[++i];
  }
  return out;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function findChrome() {
  if (process.env.CHROME_PATH) {
    return existsSync(process.env.CHROME_PATH) ? process.env.CHROME_PATH : null;
  }
  const candidates = [];
  const playwright = join(homedir(), '.cache', 'ms-playwright');
  if (existsSync(playwright)) {
    // Newest revision first: newer Chromium is more likely to run headless.
    for (const rev of readdirSync(playwright).sort().reverse()) {
      for (const rel of [
        'chrome-linux64/chrome',
        'chrome-linux/chrome',
        'chrome-headless-shell-linux64/chrome-headless-shell',
        'chrome-mac/Chromium.app/Contents/MacOS/Chromium',
      ]) {
        candidates.push(join(playwright, rev, rel));
      }
    }
  }
  candidates.push(
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/snap/bin/chromium',
  );
  for (const bin of ['google-chrome', 'chromium', 'chromium-browser', 'chrome']) {
    for (const dir of (process.env.PATH ?? '').split(':')) {
      if (dir) candidates.push(join(dir, bin));
    }
  }
  return candidates.find((c) => existsSync(c)) ?? null;
}

/// Minimal CDP client over one flat session.
class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.handlers = new Map();
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id !== undefined) {
        const p = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (!p) return;
        if (msg.error) p.reject(new Error(`${p.method}: ${msg.error.message}`));
        else p.resolve(msg.result);
      } else {
        for (const h of this.handlers.get(msg.method) ?? []) h(msg.params ?? {});
      }
    });
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.ws.send(JSON.stringify(payload));
    });
  }

  on(method, handler) {
    if (!this.handlers.has(method)) this.handlers.set(method, []);
    this.handlers.get(method).push(handler);
  }

  once(method, ms) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out waiting for ${method}`)), ms);
      this.on(method, (params) => {
        clearTimeout(timer);
        resolve(params);
      });
    });
  }
}

// Installed before any document script runs, so it observes the inline loader
// that the checks below are about. A listener added after load would always miss
// `TrunkApplicationStarted`.
const PROBE = `(() => {
  const p = { started: false, startedAtMs: null, errors: [], rejections: [] };
  window.__browserCheckProbe = p;
  addEventListener('TrunkApplicationStarted', () => {
    p.started = true;
    p.startedAtMs = Math.round(performance.now());
  });
  addEventListener('error', (e) => p.errors.push(String(e.message || e.error || e)));
  addEventListener('unhandledrejection', (e) => p.rejections.push(String(e.reason)));
})();`;

const READ_STATE = `(() => {
  const p = window.__browserCheckProbe || {};
  const app = document.querySelector('#app');
  return {
    started: p.started === true,
    startedAtMs: p.startedAtMs,
    errors: p.errors || [],
    rejections: p.rejections || [],
    wasmBindings: typeof window.wasmBindings === 'object' && window.wasmBindings !== null,
    appConfig: typeof window.APP_CONFIG === 'object' && window.APP_CONFIG !== null,
    bodyChildren: document.body.children.length,
    appChildren: app ? app.childElementCount : null,
    mainContent: !!document.querySelector('#main-content'),
    header: !!document.querySelector('header'),
    title: document.title,
  };
})()`;

async function main() {
  const chromeBin = findChrome();
  if (!chromeBin) {
    report({ skipped: 'no Chrome/Chromium binary found (set CHROME_PATH to override)' });
    return 0;
  }

  const userDataDir = mkdtempSync(join(tmpdir(), 'browser-check-'));
  const chrome = spawn(
    chromeBin,
    [
      '--headless=new',
      '--disable-gpu',
      '--no-sandbox',
      '--disable-dev-shm-usage',
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-extensions',
      '--disable-background-networking',
      '--remote-debugging-port=0',
      `--user-data-dir=${userDataDir}`,
      'about:blank',
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );

  let wsUrl = null;
  const stderrTail = [];
  chrome.stderr.on('data', (chunk) => {
    const text = String(chunk);
    stderrTail.push(text);
    if (stderrTail.length > 20) stderrTail.shift();
    const m = /DevTools listening on (ws:\/\/\S+)/.exec(text);
    if (m) wsUrl = m[1];
  });

  let ws;
  try {
    const deadline = Date.now() + 15_000;
    while (!wsUrl && Date.now() < deadline) {
      if (chrome.exitCode !== null) {
        throw new Error(`Chrome exited with ${chrome.exitCode}: ${stderrTail.join('').trim()}`);
      }
      await sleep(50);
    }
    if (!wsUrl) throw new Error(`Chrome never reported a DevTools endpoint: ${stderrTail.join('').trim()}`);

    ws = new WebSocket(wsUrl);
    await new Promise((resolve, reject) => {
      ws.addEventListener('open', resolve, { once: true });
      ws.addEventListener('error', () => reject(new Error('DevTools websocket failed')), { once: true });
    });

    const cdp = new Cdp(ws);
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const send = (method, params) => cdp.send(method, params, sessionId);

    const responses = new Map();
    const consoleMessages = [];
    const securityEntries = [];
    cdp.on('Network.responseReceived', ({ response }) => {
      responses.set(response.url, {
        status: response.status,
        mimeType: response.mimeType,
        fromCache: Boolean(response.fromDiskCache || response.fromPrefetchCache),
        headers: response.headers ?? {},
      });
    });
    cdp.on('Runtime.consoleAPICalled', ({ type, args }) => {
      if (type === 'error' || type === 'warning') {
        consoleMessages.push({ source: 'console', level: type, text: args.map((a) => a.value ?? a.description ?? '').join(' ') });
      }
    });
    cdp.on('Runtime.exceptionThrown', ({ exceptionDetails }) => {
      consoleMessages.push({
        source: 'exception',
        level: 'error',
        text: exceptionDetails.exception?.description ?? exceptionDetails.text ?? 'exception',
      });
    });
    cdp.on('Log.entryAdded', ({ entry }) => {
      if (entry.source === 'security') securityEntries.push({ level: entry.level, text: entry.text });
      else if (entry.level === 'error') consoleMessages.push({ source: entry.source, level: entry.level, text: entry.text });
    });

    await send('Page.enable');
    await send('Runtime.enable');
    await send('Log.enable');
    await send('Network.enable');
    await send('Page.addScriptToEvaluateOnNewDocument', { source: PROBE });

    const loaded = cdp.once('Page.loadEventFired', timeoutMs);
    await send('Page.navigate', { url });
    await loaded;

    let startedAt = null;
    const startDeadline = Date.now() + timeoutMs;
    while (Date.now() < startDeadline) {
      const { result } = await send('Runtime.evaluate', {
        expression: 'window.__browserCheckProbe?.started === true',
        returnByValue: true,
      });
      if (result.value === true) {
        startedAt = Date.now();
        break;
      }
      await sleep(100);
    }
    // Let late sub-resources (lazy chunks, the SRI-checked snippet) land before
    // the resource table and the console are read.
    await sleep(startedAt ? 1_000 : 500);

    const { result } = await send('Runtime.evaluate', { expression: READ_STATE, returnByValue: true });
    const state = result.value;
    // The document response is keyed by the URL the browser actually resolved,
    // which carries the trailing slash the caller may have omitted.
    const document_ = responses.get(url)
      ?? responses.get(`${url}/`)
      ?? responses.get(url.replace(/\/$/, ''))
      ?? null;
    const csp = document_?.headers?.['content-security-policy'] ?? document_?.headers?.['Content-Security-Policy'] ?? null;

    const failures = [];
    const fail = (name, detail) => failures.push({ name, detail });

    if (!document_) fail('document', `no response observed for ${url}`);
    else if (document_.status !== 200) {
      fail('document', `${url} answered ${document_.status} ${document_.mimeType}`);
    }

    if (!state.started) {
      fail('bootstrap', 'the document never dispatched TrunkApplicationStarted — the inline Trunk loader did not run');
    }
    if (!state.wasmBindings) fail('bootstrap', 'window.wasmBindings is missing after the load event');
    if (!state.appConfig) fail('config', 'window.APP_CONFIG is missing — /static/config.js did not define it');
    if (!state.mainContent || !state.header) {
      fail('mount', `the Yew app did not render (main-content=${state.mainContent}, header=${state.header})`);
    }
    if (state.bodyChildren === 0) fail('mount', 'the document body has no children');

    const wasm = [...responses.entries()].find(([u]) => /\.wasm(\?|$)/.test(u));
    if (!wasm) fail('wasm', 'no .wasm request was made — the module was never fetched');
    else {
      const [wasmUrl, wasmRes] = wasm;
      if (wasmRes.status !== 200 && wasmRes.status !== 304) {
        fail('wasm', `${wasmUrl} answered ${wasmRes.status} ${wasmRes.mimeType}`);
      } else if (wasmRes.mimeType !== 'application/wasm') {
        // The classic regression: the SPA fallback answers a missing hashed
        // asset with index.html and the instantiation fails with a MIME error.
        fail('wasm', `${wasmUrl} was served as "${wasmRes.mimeType}", not application/wasm`);
      }
    }

    const configJs = [...responses.entries()].find(([u]) => /\/static\/config\.js(\?|$)/.test(u));
    if (!configJs) fail('config', '/static/config.js was never requested');
    else if (configJs[1].status !== 200) {
      fail('config', `/static/config.js answered ${configJs[1].status} ${configJs[1].mimeType}`);
    }

    for (const entry of [...state.errors.map((text) => ({ source: 'window', level: 'error', text })),
      ...state.rejections.map((text) => ({ source: 'window', level: 'rejection', text }))]) {
      fail('script-error', `${entry.source}: ${entry.text}`);
    }
    for (const entry of consoleMessages) {
      fail('console', `${entry.source} ${entry.level}: ${entry.text}`);
    }
    for (const entry of securityEntries) {
      // Chrome reports both CSP refusals and Subresource-Integrity failures with
      // `source: "security"`, and neither is distinguishable by name alone.
      fail('security', `${entry.level}: ${entry.text}`);
    }
    if (csp && /script-src[^;]*/.test(csp) && !/script-src[^;]*'unsafe-inline'/.test(csp)) {
      fail('csp', "script-src lost 'unsafe-inline'; the Trunk loader is an inline module and cannot run");
    }

    report({
      url,
      chrome: chromeBin,
      csp,
      document: document_ ? `${document_.status} ${document_.mimeType}` : null,
      bootstrapMs: state.startedAtMs,
      wasm: wasm ? `${wasm[1].status} ${wasm[1].mimeType}` : null,
      wasmUrl: wasm?.[0] ?? null,
      requests: responses.size,
      title: state.title,
      failures,
    });
    return failures.length === 0 ? 0 : 1;
  } finally {
    try { ws?.close(); } catch { /* already closed */ }
    chrome.kill('SIGKILL');
    rmSync(userDataDir, { recursive: true, force: true });
  }
}

function report(data) {
  if (asJson) {
    process.stdout.write(`${JSON.stringify(data, null, 2)}\n`);
    return;
  }
  if (data.skipped) {
    process.stdout.write(`browser-check skipped: ${data.skipped}\n`);
    return;
  }
  const line = (k, v) => process.stdout.write(`  ${k.padEnd(10)} ${v}\n`);
  process.stdout.write(`browser-check ${data.url}\n`);
  line('chrome', data.chrome);
  line('document', data.document ?? '(no response)');
  line('csp', data.csp ?? '(none sent)');
  line('bootstrap', data.bootstrapMs === null ? 'did not start' : `after ${data.bootstrapMs} ms`);
  line('wasm', data.wasm ? `${data.wasm} ${data.wasmUrl}` : 'not requested');
  line('requests', String(data.requests));
  if (data.failures.length === 0) {
    process.stdout.write('  PASS\n');
  } else {
    for (const f of data.failures) process.stdout.write(`  FAIL ${f.name}: ${f.detail}\n`);
    process.stdout.write(`  ${data.failures.length} failure(s)\n`);
  }
}

process.exitCode = await main().catch((err) => {
  process.stderr.write(`browser-check failed to run: ${err.message}\n`);
  return 1;
});
