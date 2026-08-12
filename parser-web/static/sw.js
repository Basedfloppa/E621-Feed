// static/sw.js — E621 Feed Service Worker.
//
// Registered at the root scope (`/sw.js`, scope `/`). Responsibilities:
//   * offline fallback — navigations that fail while offline and have no
//     cached app shell are answered with `offline.html` instead of a blank
//     white screen.
//   * cache-first static assets (wasm/css/js/static) on repeat visits.
//   * cache API responses for offline browsing.
//   * background sync for queued feedback + periodic self-refresh.
//
// Bump CACHE_VERSION whenever pre-cache or cache names change. Must be
// bumped on every frontend deploy that can affect asset bytes — the static
// cache holds entries keyed by URL, and a bumped version is the only way to
// purge entries that went stale (see the SRI notes below).

const CACHE_VERSION = "e621-static-v3";
const STATIC_CACHE = `${CACHE_VERSION}`;

// API content endpoints worth keeping for offline browsing. Cached responses
// are stored in IndexedDB and served network-first: fresh when online, last
// known good when offline.
const API_CACHEABLE = [
	"/api/recommendations/",
	"/api/browse/trending",
	"/api/browse/search",
	"/api/browse/favorites",
	"/api/digest/",
	"/api/posts/",
];

// IndexedDB schema: cached API responses and pending interaction events
// awaiting background-sync replay.
const DB_NAME = "e621-feed";
const DB_VERSION = 2;
const API_STORE = "api-responses";
const PENDING_STORE = "pending-events";

// Background-sync tag used to flush queued interactions when online.
const SYNC_TAG = "feed-events";

// Stable, version-independent files that must exist even on the very first
// offline visit (they have fixed names across builds).
const PRECACHE_URLS = ["/static/offline.html"];

self.addEventListener("install", (event) => {
	event.waitUntil(
		(async () => {
			const cache = await caches.open(STATIC_CACHE);
			await cache.addAll(PRECACHE_URLS);
			await self.skipWaiting();
		})(),
	);
});

self.addEventListener("activate", (event) => {
	const clearStale = (async () => {
		const keys = await caches.keys();
		await Promise.all(
			keys
				.filter((k) => !k.startsWith(CACHE_VERSION))
				.map((k) => caches.delete(k)),
		);
	})();
	event.waitUntil(Promise.all([self.clients.claim(), clearStale]));
});

self.addEventListener("fetch", (event) => {
	let url;
	try {
		url = new URL(event.request.url);
	} catch (err) {
		return; // Malformed URL — ignore.
	}
	if (url.origin !== self.location.origin) {
		return;
	}

	// Interaction feedback (open/hide/like…): if the network is down, queue
	// the POST in IndexedDB and schedule a background sync to replay it later
	// instead of dropping the user's feedback.
	if (event.request.method === "POST" && isInteractionUrl(url.pathname)) {
		event.respondWith(handleInteractionPost(event.request));
		return;
	}

	// Only GET requests are routed for caching below.
	if (event.request.method !== "GET") {
		return;
	}

	// Offline fallback for page navigations. A navigation is answered
	// network-first so the freshest app shell is used when online; if it fails
	// while offline, serve a cached app shell (index) or the offline page.
	if (event.request.mode === "navigate") {
		event.respondWith(handleNavigation(event.request));
		return;
	}

	// API responses: network-first with an IndexedDB offline fallback.
	if (url.pathname.startsWith("/api/")) {
		if (API_CACHEABLE.some((p) => url.pathname.startsWith(p))) {
			event.respondWith(networkFirstApi(event.request));
		}
		// Non-cacheable API calls use the default browser fetch.
		return;
	}

	// Static assets (wasm/css/js/static): cache-first so repeat visits are
	// instant and the app keeps working offline once assets are cached.
	// Hashed bundles carry an `integrity` attribute — see cacheFirst for why
	// those are verified against their digest before serving from cache.
	event.respondWith(cacheFirst(event.request));
});

// Map an `integrity` attribute token to a WebCrypto digest algorithm name.
function integrityAlgo(value) {
	if (value.startsWith("sha256-")) return "SHA-256";
	if (value.startsWith("sha384-")) return "SHA-384";
	if (value.startsWith("sha512-")) return "SHA-512";
	return null;
}

// Base64 (standard, padded) of a byte array, chunked so multi-megabyte wasm
// bundles don't overflow the call stack via String.fromCharCode(...spread).
function bytesToBase64(bytes) {
	let binary = "";
	const CHUNK = 0x8000;
	for (let i = 0; i < bytes.length; i += CHUNK) {
		binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
	}
	return btoa(binary);
}

// True when `response` satisfies the request's SRI `integrity` metadata
// ("sha384-<base64>", possibly a space-separated list of algorithms).
// A response fetched without an integrity requirement always passes.
async function passesIntegrity(response, integrity) {
	if (!integrity) {
		return true;
	}
	for (const part of integrity.trim().split(/\s+/)) {
		const algo = integrityAlgo(part);
		if (!algo) {
			continue; // unsupported token — treated as "no matching hash"
		}
		try {
			const expected = part.slice(part.indexOf("-") + 1);
			const bytes = await response.clone().arrayBuffer();
			const digest = await crypto.subtle.digest(algo, bytes);
			if (bytesToBase64(new Uint8Array(digest)) === expected) {
				return true;
			}
		} catch (err) {
			return false;
		}
	}
	return false;
}

// Cache-first with SRI self-healing.
//
// The hashed bundles (wasm/js/css) are named by trunk with a weak 64-bit hash
// of the *wasm* only — not of each file's own bytes — so two builds can reuse
// the same URL while the file content differs (toolchain drift, a rebuild
// whose wasm happens to match, or a hash collision). Served stale, that copy
// makes the browser's SRI check fail loudly with "None of the sha384 hashes
// in the integrity attribute match the content of the subresource…". To
// prevent that:
//   * a cached copy is served only if it still matches the request's
//     `integrity` metadata;
//   * on mismatch the entry is dropped and re-fetched bypassing the HTTP
//     cache, so the client self-heals on the very next load instead of
//     staying stuck on the stale bytes for the cache lifetime;
//   * the fresh copy is cached only if it too passes SRI, so the cache is
//     never re-poisoned with bytes that fail the page's integrity check.
async function cacheFirst(request) {
	const cache = await caches.open(STATIC_CACHE);
	const cached = await cache.match(request);
	if (cached && (await passesIntegrity(cached, request.integrity))) {
		return cached;
	}
	if (cached) {
		await cache.delete(request);
	}
	const response = await fetch(request, {
		// The browser's HTTP cache may hold the same stale bytes we just
		// rejected — bypass it so the re-fetch goes to the server.
		cache: request.integrity ? "no-cache" : "default",
	});
	if (response && (response.status === 200 || response.type === "opaque")) {
		if (await passesIntegrity(response, request.integrity)) {
			cache.put(request, response.clone());
		}
	}
	return response;
}

// ── IndexedDB-backed API caching ──────────────────────────────────────

function openDB() {
	return new Promise((resolve, reject) => {
		const req = indexedDB.open(DB_NAME, DB_VERSION);
		req.onupgradeneeded = (e) => {
			const db = e.target.result;
			if (!db.objectStoreNames.contains(API_STORE)) {
				db.createObjectStore(API_STORE, { keyPath: "url" });
			}
			if (!db.objectStoreNames.contains(PENDING_STORE)) {
				db.createObjectStore(PENDING_STORE, { keyPath: "id" });
			}
		};
		req.onsuccess = () => resolve(req.result);
		req.onerror = () => reject(req.error);
	});
}

function txDone(tx) {
	return new Promise((resolve, reject) => {
		tx.oncomplete = () => resolve();
		tx.onerror = () => reject(tx.error);
		tx.onabort = () => reject(tx.error);
	});
}

async function storeApiResponse(url, response) {
	try {
		const body = await response.clone().text();
		const contentType =
			response.headers.get("content-type") || "application/json";
		const db = await openDB();
		const tx = db.transaction(API_STORE, "readwrite");
		tx.objectStore(API_STORE).put({
			url,
			body,
			contentType,
			ts: Date.now(),
		});
		await txDone(tx);
		db.close();
	} catch (err) {
		// Caching is best-effort; never break the live request.
	}
}

function readApiResponse(url) {
	return openDB().then(
		(db) =>
			new Promise((resolve) => {
				const tx = db.transaction(API_STORE, "readonly");
				const req = tx.objectStore(API_STORE).get(url);
				req.onsuccess = () => {
					const value = req.result || null;
					db.close();
					resolve(value);
				};
				req.onerror = () => {
					db.close();
					resolve(null);
				};
			}),
		() => null,
	);
}

function fromStoredApi(value) {
	return new Response(value.body, {
		status: 200,
		headers: { "Content-Type": value.contentType },
	});
}

async function networkFirstApi(request) {
	try {
		const response = await fetch(request);
		if (response && response.ok) {
			storeApiResponse(request.url, response.clone());
			return response;
		}
		// Non-2xx (e.g. 429/5xx) — fall back to last-known-good so browsing keeps
		// working during transient failures.
		const cached = await readApiResponse(request.url);
		if (cached) {
			return fromStoredApi(cached);
		}
		return response;
	} catch (err) {
		// Offline or a transient blip in the service worker's own network stack
		// (stale keep-alive, connection reset, worker replacement during deploy).
		// Serve the last cached response if any; otherwise hand the page a clean
		// 503 so the app's own error/retry UI handles it. We must NEVER reject
		// `respondWith` — a rejected respondWith surfaces as a confusing
		// "ServiceWorker unexpected error" and hard-blocks the request even
		// though the backend/app are perfectly healthy.
		const cached = await readApiResponse(request.url).catch(() => null);
		if (cached) {
			return fromStoredApi(cached);
		}
		return new Response(
			JSON.stringify({ error: "network_unavailable", code: 503 }),
			{
				status: 503,
				statusText: "Service Unavailable",
				headers: { "Content-Type": "application/json" },
			},
		);
	}
}

async function handleNavigation(request) {
	try {
		const fresh = await fetch(request);
		// Opportunistically cache the app shell for future offline navigations.
		const cache = await caches.open(STATIC_CACHE);
		cache.put(request.url, fresh.clone());
		return fresh;
	} catch (err) {
		const cache = await caches.open(STATIC_CACHE);
		const cachedIndex = await cache.match(request.url);
		if (cachedIndex) {
			return cachedIndex;
		}
		const offline = await cache.match("/static/offline.html");
		if (offline) {
			return offline;
		}
		// Nothing cached at all — let the browser show its own error page.
		throw err;
	}
}

// ── Background sync for interaction feedback ─────────────────────────

function isInteractionUrl(pathname) {
	return (
		pathname === "/api/interaction" || pathname === "/api/interaction/batch"
	);
}

function uniqueId() {
	return `e-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function storeOperation(store, mode, fn) {
	return openDB().then(
		(db) =>
			new Promise((resolve) => {
				const tx = db.transaction(store, mode);
				fn(tx.objectStore(store));
				tx.oncomplete = () => {
					db.close();
					resolve();
				};
				tx.onerror = () => {
					db.close();
					resolve();
				};
			}),
		() => {},
	);
}

function enqueuePending(record) {
	return storeOperation(PENDING_STORE, "readwrite", (s) => {
		s.put({ id: uniqueId(), ...record });
	});
}

function getAllPending() {
	return openDB().then(
		(db) =>
			new Promise((resolve) => {
				const tx = db.transaction(PENDING_STORE, "readonly");
				const req = tx.objectStore(PENDING_STORE).getAll();
				req.onsuccess = () => {
					const values = req.result || [];
					db.close();
					resolve(values);
				};
				req.onerror = () => {
					db.close();
					resolve([]);
				};
			}),
		() => [],
	);
}

function removePending(id) {
	return storeOperation(PENDING_STORE, "readwrite", (s) => s.delete(id));
}

function scheduleSync() {
	try {
		if (self.registration && self.registration.sync) {
			self.registration.sync
				.register(SYNC_TAG)
				.catch((err) => console.warn("[pwa] background sync register:", err));
		}
	} catch (err) {
		// Background Sync unsupported — feedback is dropped while offline.
	}
}

async function handleInteractionPost(request) {
	try {
		// Network is up — forward normally.
		return await fetch(request);
	} catch (err) {
		// Offline (network-level failure). Persist and replay later.
		try {
			const body = await request.clone().text();
			const contentType =
				request.headers.get("content-type") || "application/json";
			await enqueuePending({
				url: request.url,
				method: request.method,
				body,
				contentType,
				ts: Date.now(),
			});
		} catch (qerr) {
			// Could not persist — nothing more we can do.
		}
		scheduleSync();
		// Tell the app the feedback is accepted; it will be delivered later.
		return new Response(null, { status: 202, statusText: "Queued" });
	}
}

async function replayPendingEvents() {
	const events = await getAllPending();
	let flushed = 0;
	for (const ev of events) {
		try {
			const resp = await fetch(ev.url, {
				method: ev.method,
				headers: ev.contentType
					? { "Content-Type": ev.contentType }
					: undefined,
				body: ev.body,
				credentials: "include",
			});
			if (resp.ok) {
				await removePending(ev.id);
				flushed += 1;
			}
			// Non-2xx — keep the event for a later retry.
		} catch (err) {
			// Network flapped mid-replay — keep for the next sync.
		}
	}
	if (flushed > 0) {
		clients.matchAll({ includeUncontrolled: true }).then((list) => {
			list.forEach((c) => {
				c.postMessage({ type: "feed-events-flushed", count: flushed });
			});
		});
	}
}

self.addEventListener("sync", (event) => {
	if (event.tag === SYNC_TAG) {
		event.waitUntil(replayPendingEvents());
	}
});

// ── Periodic background sync ─────────────────────────────────────────
// Fires roughly every `minInterval` while the (installed) PWA is idle, so a
// returning user is served fresh content: flush queued interactions and
// re-hydrate the app shell + core static assets in place.

const PERIODIC_TAG = "feed-refresh";
// 6 hours.
const PERIODIC_INTERVAL_MS = 6 * 60 * 60 * 1000;

async function refreshPeriodically() {
	// Deliver any queued feedback first.
	await replayPendingEvents();

	// Re-validate the app shell and static shell so installed clients pick up
	// new deployments without a manual refresh.
	try {
		const cache = await caches.open(STATIC_CACHE);
		for (const url of ["/", "/static/offline.html"]) {
			try {
				const resp = await fetch(url, {
					cache: "reload",
					credentials: "same-origin",
				});
				if (resp && resp.ok) {
					cache.put(url, resp);
				}
			} catch (err) {
				// Offline — keep the previous cached copy.
			}
		}
	} catch (err) {
		// Best-effort.
	}
}

self.addEventListener("periodicsync", (event) => {
	if (event.tag === PERIODIC_TAG) {
		event.waitUntil(refreshPeriodically());
	}
});
