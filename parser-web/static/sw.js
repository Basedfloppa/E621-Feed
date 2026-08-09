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
// Bump CACHE_VERSION whenever pre-cache or cache names change.

const CACHE_VERSION = "e621-static-v2";
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
	event.respondWith(cacheFirst(event.request));
});

async function cacheFirst(request) {
	const cache = await caches.open(STATIC_CACHE);
	const cached = await cache.match(request);
	if (cached) {
		return cached;
	}
	const response = await fetch(request);
	if (response && (response.status === 200 || response.type === "opaque")) {
		cache.put(request, response.clone());
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
		// Offline — serve the last cached response, if any.
		const cached = await readApiResponse(request.url);
		if (cached) {
			return fromStoredApi(cached);
		}
		throw err;
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
