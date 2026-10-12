#!/usr/bin/env node
// HTTP smoke test for a running parser-api instance: the embedded SPA, the
// static-asset contract and the owner-cookie session flow.
//
// Everything here is checkable with `curl`, which is the point: these are the
// contracts that broke in production and that no Rust test can see, because they
// are decided by the HTTP layer rather than by the code that builds the response.
//
//   * `/metrics` answered `index.html` with status 200, so a scraper pointed at
//     the wrong path collected HTML and reported the target as up (§4.2);
//   * a missing hashed asset was answered by the SPA fallback with HTML, and the
//     browser then refused the module with a MIME error;
//   * the WASM and CSS carry Subresource-Integrity hashes from the build, so a
//     partial deploy (new `index.html`, old assets) breaks the app silently —
//     the hashes are verified here against the bytes actually served.
//
// Usage:
//   node scripts/e2e-smoke.mjs --base-url http://127.0.0.1:8181
//   node scripts/e2e-smoke.mjs --json
//
// Environment: E2E_BASE_URL, E2E_TIMEOUT.

import { createHash } from 'node:crypto';

const opts = parseArgs(process.argv.slice(2));
const baseUrl = (opts['base-url'] ?? process.env.E2E_BASE_URL ?? 'http://127.0.0.1:8181').replace(/\/$/, '');
const timeoutMs = Number(opts.timeout ?? process.env.E2E_TIMEOUT ?? 30_000);
const asJson = Boolean(opts.json);

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--json') out.json = true;
    else if (a.startsWith('--')) out[a.slice(2)] = argv[i + 1];
  }
  return out;
}

async function request(path, init = {}) {
  const target = path.startsWith('http') ? path : `${baseUrl}${path}`;
  const res = await fetch(target, { redirect: 'manual', signal: AbortSignal.timeout(timeoutMs), ...init });
  const body = await res.text();
  return { res, body, status: res.status, type: res.headers.get('content-type') ?? '' };
}

async function requestBytes(url) {
  const res = await fetch(url, { redirect: 'manual', signal: AbortSignal.timeout(timeoutMs) });
  return { res, bytes: Buffer.from(await res.arrayBuffer()) };
}

function expect(cond, message) {
  if (!cond) throw new Error(message);
}

const results = [];

async function check(name, fn) {
  try {
    const detail = await fn();
    if (detail && typeof detail === 'object' && detail.skip) {
      results.push({ name, ok: true, skipped: true, detail: detail.skip });
    } else {
      results.push({ name, ok: true, detail: detail ?? '' });
    }
  } catch (err) {
    results.push({ name, ok: false, detail: err.message });
  }
}

const skip = (reason) => ({ skip: reason });

const SRI_RE = /<(?:link|script)\b[^>]*>/gi;
const ATTR_RE = (tag, attr) => new RegExp(`${attr}\\s*=\\s*"([^"]*)"`, 'i');

function referencedAssets(html) {
  const assets = [];
  for (const tag of html.match(SRI_RE) ?? []) {
    for (const key of ['href', 'src']) {
      const m = ATTR_RE(tag, key).exec(tag);
      if (!m) continue;
      const integrity = ATTR_RE(tag, 'integrity').exec(tag)?.[1] ?? null;
      const raw = m[1];
      if (/^https?:|^\/\//.test(raw)) continue;
      // `index.html` mixes root-absolute (`/style-….css`) and document-relative
      // (`./static/vendor/shepherd.css`) references; both must resolve.
      assets.push({ url: new URL(raw, `${baseUrl}/`).toString(), integrity });
    }
  }
  return assets;
}

function verifySri(body, integrity) {
  const [alg, expected] = integrity.split('-');
  const digest = createHash(alg).update(body).digest('base64');
  return digest === expected;
}

await check('/ serves the embedded SPA', async () => {
  const { status, type, body } = await request('/');
  expect(status === 200, `expected 200, got ${status}`);
  expect(type.includes('text/html'), `expected text/html, got "${type}"`);
  expect(!body.includes('Frontend not embedded'), 'the API binary was built without parser-web/dist');
  expect(/TrunkApplicationStarted/.test(body), 'the Trunk inline bootstrap is missing from index.html');
  expect(/_bg\.wasm/.test(body), 'index.html does not reference a wasm bundle');
  return `${(body.length / 1024).toFixed(1)} kB`;
});

const indexHtml = await request('/').then((r) => r.body).catch(() => '');
const assets = referencedAssets(indexHtml);

await check('every referenced bundle asset is served with a valid SRI hash', async () => {
  expect(assets.length > 0, 'no <link>/<script> assets found in index.html');
  const problems = [];
  let verified = 0;
  for (const asset of assets) {
    // Bytes, not text: reading a `.wasm` as UTF-8 replaces invalid sequences
    // and every hash over it would mismatch.
    const { res, bytes } = await requestBytes(asset.url);
    if (res.status !== 200) {
      problems.push(`${asset.url} -> ${res.status}`);
      continue;
    }
    if (asset.integrity) {
      if (!verifySri(bytes, asset.integrity)) problems.push(`${asset.url} failed ${asset.integrity}`);
      else verified += 1;
    }
  }
  expect(problems.length === 0, problems.join('; '));
  return `${assets.length} assets, ${verified} integrity hashes verified`;
});

await check('the wasm bundle is served as application/wasm and cached immutably', async () => {
  const wasmUrl = assets.map((a) => a.url).find((u) => u.endsWith('.wasm'))
    ?? /\/[\w.-]+_bg\.wasm/.exec(indexHtml)?.[0];
  expect(Boolean(wasmUrl), 'no .wasm reference found in index.html');
  const { res, status, type } = await request(wasmUrl);
  expect(status === 200, `${wasmUrl} answered ${status}`);
  // A MIME mismatch is what makes instantiateStreaming fail; the SPA fallback
  // answering a missing asset with text/html is the usual cause.
  expect(type.includes('application/wasm'), `${wasmUrl} served as "${type}"`);
  const cache = res.headers.get('cache-control') ?? '';
  expect(cache.includes('immutable'), `${wasmUrl} is not cached immutably ("${cache}")`);
  return `${type}, ${cache}`;
});

await check('/static/config.js is runtime config, never cached', async () => {
  const { res, status, body } = await request('/static/config.js');
  expect(status === 200, `expected 200, got ${status}`);
  expect(body.includes('APP_CONFIG'), 'config.js does not define window.APP_CONFIG');
  const cache = res.headers.get('cache-control') ?? '';
  expect(cache.includes('no-store'), `mutable config is cacheable ("${cache}")`);
  return cache;
});

await check('/metrics is a 404, not the SPA fallback', async () => {
  const { status, type, body } = await request('/metrics');
  expect(status === 404, `/metrics answered ${status}`);
  expect(!body.includes('<html'), '/metrics answered an HTML document');
  return `${status} ${type}`;
});

await check('/api/metrics exposes Prometheus metrics', async () => {
  const { status, type, body } = await request('/api/metrics');
  expect(status === 200, `/api/metrics answered ${status}`);
  expect(body.includes('e621_'), 'no e621_* metric families in the payload');
  return `${body.split('\n').filter((l) => l.startsWith('e621_')).length} metric lines`;
});

await check('/api/health reports database, caches and e621', async () => {
  const { status, body } = await request('/api/health');
  // 503 is the documented answer when a probe fails or the probe rate limit is
  // hit; the check is about the *shape* of the report, not about e621 being up.
  expect(status === 200 || status === 503, `/api/health answered ${status}`);
  const json = JSON.parse(body);
  for (const key of ['database', 'caches', 'e621']) {
    expect(key in json, `/api/health has no "${key}" field`);
  }
  return `${status} ${JSON.stringify(json)}`;
});

await check('unknown client routes fall back to the SPA entry', async () => {
  const { status, type, body } = await request('/some/deep/client/route');
  expect(status === 200, `expected 200, got ${status}`);
  expect(type.includes('text/html'), `expected text/html, got "${type}"`);
  expect(body.includes('TrunkApplicationStarted'), 'the fallback did not return index.html');
  return `${status} ${type}`;
});

await check('the SPA fallback does not swallow missing files', async () => {
  const { status } = await request('/missing-asset-9d3f.js');
  expect(status === 404, `a missing .js asset answered ${status} — a stale index.html would load HTML as a module`);
  return `${status}`;
});

await check('unknown /api routes answer JSON, not HTML', async () => {
  const { status, type, body } = await request('/api/no-such-endpoint');
  expect(status === 404, `expected 404, got ${status}`);
  expect(!body.includes('<html'), '/api answered an HTML document');
  return `${status} ${type}`;
});

await check('a protected route requires the owner cookie', async () => {
  const { status } = await request('/api/accounts');
  expect(status === 401, `expected 401 without a cookie, got ${status}`);
  return `${status} without cookie`;
});

await check('a malformed owner cookie is rejected as a bad request', async () => {
  const { status } = await request('/api/accounts', { headers: { cookie: 'owner_token=not-a-valid-token!' } });
  expect(status === 400, `expected 400 for a malformed token, got ${status}`);
  return `${status} for a malformed token`;
});

// Debug builds mount a `rocket_cors` fairing that only allows the `trunk serve`
// origin and answers any other one with the `/cors/403` route, which the SPA
// fallback then serves as `200 OK` + `index.html` (TODO §11.8). Release builds
// have no fairing, and `/api/openapi.json` is mounted only in debug, so its
// presence identifies the build.
const debugBuild = await request('/api/openapi.json').then((r) => r.status === 200).catch(() => false);

await check('session/bootstrap mints a cookie that authenticates the API', async () => {
  // A browser sends `Origin` on an unsafe method, and in a release build
  // `csrf_origin_is_allowed` demands it match the request host. A debug build
  // tolerates a missing `Origin` but would have the request diverted by the
  // debug-only CORS fairing if one were sent, so the header is mode-dependent.
  const headers = debugBuild ? {} : { origin: baseUrl };
  const boot = await request('/api/session/bootstrap', { method: 'POST', headers });
  expect(boot.status === 200, `bootstrap answered ${boot.status}`);
  const setCookies = boot.res.headers.getSetCookie?.() ?? [];
  const cookie = setCookies.find((c) => c.startsWith('owner_token='));
  expect(Boolean(cookie), 'bootstrap did not set an owner_token cookie');
  for (const attr of ['HttpOnly', 'SameSite=Strict', 'Path=/api']) {
    expect(cookie.includes(attr), `the owner cookie is missing ${attr}`);
  }
  const token = cookie.split(';')[0].split('=').slice(1).join('=');
  const accounts = await request('/api/accounts', { headers: { cookie: `owner_token=${token}` } });
  expect(accounts.status === 200, `/api/accounts with a minted cookie answered ${accounts.status}`);
  const json = JSON.parse(accounts.body);
  expect(Array.isArray(json), '/api/accounts did not return a JSON array');
  return `minted token authenticates, ${json.length} account(s)`;
});

await check('session/state reports what claiming the instance would link', async () => {
  // The settings card reads this before offering the secret field, and it must
  // answer for a visitor who has no cookie yet — otherwise the card could not
  // decide between the enroll form and the "this instance is open" note.
  const { status, body, type } = await request('/api/session/state');
  expect(status === 200, `session/state answered ${status}`);
  expect(type.includes('application/json'), `session/state answered ${type}`);
  const json = JSON.parse(body);
  for (const field of ['instance_locked', 'owned_accounts', 'adoptable_accounts']) {
    expect(field in json, `session/state is missing ${field}`);
  }
  expect(typeof json.instance_locked === 'boolean', 'instance_locked is not a boolean');
  expect(Number.isInteger(json.adoptable_accounts), 'adoptable_accounts is not an integer');
  return `locked=${json.instance_locked} owned=${json.owned_accounts} adoptable=${json.adoptable_accounts}`;
});

await check('a request carrying the app origin is not answered with index.html', async () => {
  if (debugBuild) {
    return skip('debug build: rocket_cors redirects non-allowlisted origins to /cors/403, which the SPA fallback answers with index.html (TODO §11.8); run against a release build to exercise this');
  }
  const { status, type, body } = await request('/static/config.js', { headers: { origin: baseUrl } });
  expect(status === 200, `/static/config.js answered ${status} with the app origin set`);
  expect(!body.includes('<html'), 'the asset was replaced by the SPA fallback');
  expect(type.includes('javascript'), `expected javascript, got "${type}"`);
  return `${status} ${type}`;
});

const failures = results.filter((r) => !r.ok);

if (asJson) {
  process.stdout.write(`${JSON.stringify({ baseUrl, results, failures: failures.length }, null, 2)}\n`);
} else {
  process.stdout.write(`e2e-smoke ${baseUrl}\n`);
  for (const r of results) {
    const mark = r.skipped ? 'skip' : r.ok ? 'ok  ' : 'FAIL';
    process.stdout.write(`  ${mark} ${r.name}${r.detail ? ` — ${r.detail}` : ''}\n`);
  }
  const skipped = results.filter((r) => r.skipped).length;
  process.stdout.write(
    `  ${results.length - failures.length - skipped}/${results.length - skipped} checks passed${skipped ? `, ${skipped} skipped` : ''}\n`,
  );
}

process.exitCode = failures.length === 0 ? 0 : 1;
