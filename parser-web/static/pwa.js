// static/pwa.js — PWA bootstrap for E621 Feed.
//
// Registers the service worker (loaded at the root scope) and wires up:
//   * the install prompt (`beforeinstallprompt` → a dismissible install
//     banner),
//   * a `pwa-installable` CustomEvent on `window` so the (Yew) UI could
//     optionally integrate its own install affordance.
// Kept as a small plain script so it runs before the (large, hashed) WASM
// bundle finishes loading.

(() => {
	"use strict";

	// No service-worker support (older Safari/private browsing).
	if (!("serviceWorker" in navigator)) {
		console.warn("[pwa] Service workers not supported; PWA features disabled.");
		return;
	}

	const isStandalone =
		window.matchMedia("(display-mode: standalone)").matches ||
		window.navigator.standalone === true;

	// Register the service worker once the page has loaded.
	window.addEventListener("load", () => {
		navigator.serviceWorker
			.register("/sw.js", { scope: "/" })
			.then(() => {
				// Registration is async and not awaited by the UI; nothing to do yet.
			})
			.catch((err) => {
				console.warn("[pwa] Service worker registration failed:", err);
			});
	});

	// ── Install prompt ─────────────────────────────────────────────────
	// The browser fires `beforeinstallprompt` on (potentially) every load, but
	// we only show our banner once per device (one-shot). After "Later" it
	// never auto-shows again; the user can re-enable / trigger it from the
	// settings (Storage / Offline card) via the toggle and the manual button.
	const INSTALL_ENABLED_KEY = "pwa_install_enabled"; // "1" default
	const INSTALL_DISMISSED_KEY = "pwa_install_dismissed"; // "1" = one-shot used

	let deferredPrompt = null;
	let banner = null;
	let bannerShown = false;

	function readFlag(key, def) {
		try {
			const v = localStorage.getItem(key);
			return v === null ? def : v === "1";
		} catch (err) {
			return def;
		}
	}

	function writeFlag(key, val) {
		try {
			localStorage.setItem(key, val ? "1" : "0");
		} catch (err) {
			/* best-effort */
		}
	}

	// One-shot check: hide the auto-banner if it was already dismissed, or if
	// the user disabled the prompt in settings.
	function autoBannerAllowed() {
		if (!readFlag(INSTALL_ENABLED_KEY, true)) return false;
		if (readFlag(INSTALL_DISMISSED_KEY, false)) return false;
		return true;
	}

	function showInstallBanner() {
		if (bannerShown || banner || isStandalone) return;
		if (!autoBannerAllowed()) return;
		bannerShown = true;

		// Theme-aware banner: uses the app's own daisyUI classes so it matches
		// whatever theme the user selected, instead of hardcoded colours.
		banner = document.createElement("div");
		banner.className =
			"fixed bottom-4 left-1/2 -translate-x-1/2 z-[9999] flex items-center gap-3 ";
		banner.className +=
			"rounded-full border border-base-300 bg-base-100 px-4 py-2 shadow-lg text-base-content";
		banner.setAttribute("role", "dialog");
		banner.setAttribute("aria-label", "Install E621 Feed as an app");

		const note = document.createElement("span");
		note.className = "text-sm text-base-content/80 truncate max-w-[240px]";
		note.textContent = "Install E621 Feed as an app";

		const installBtn = document.createElement("button");
		installBtn.type = "button";
		installBtn.className = "btn btn-primary btn-sm";
		installBtn.textContent = "Install";

		const dismissBtn = document.createElement("button");
		dismissBtn.type = "button";
		dismissBtn.className = "btn btn-ghost btn-sm";
		dismissBtn.textContent = "Later";
		dismissBtn.setAttribute("aria-label", "Dismiss");

		banner.append(note);
		banner.append(installBtn);
		banner.append(dismissBtn);
		document.body.appendChild(banner);

		installBtn.addEventListener("click", async () => {
			if (!deferredPrompt) {
				hideInstallBanner();
				return;
			}
			deferredPrompt.prompt();
			try {
				await deferredPrompt.userChoice;
			} catch (err) {
				/* user dismissed the prompt */
			}
			deferredPrompt = null;
			hideInstallBanner();
		});

		dismissBtn.addEventListener("click", () => {
			// One-shot: consuming "Later" means we never auto-ask again.
			writeFlag(INSTALL_DISMISSED_KEY, true);
			hideInstallBanner();
		});
	}

	function hideInstallBanner() {
		if (banner) {
			banner.remove();
			banner = null;
		}
	}

	// The browser decides when the app is installable; capture the event and
	// surface the banner according to the one-shot / settings rules.
	window.addEventListener("beforeinstallprompt", (event) => {
		event.preventDefault();
		deferredPrompt = event;
		window.dispatchEvent(new CustomEvent("pwa-installable"));
		if (autoBannerAllowed()) {
			showInstallBanner();
		}
	});

	// Manual trigger from the settings page: opens the native prompt if a
	// deferred prompt is available, otherwise re-surfaces our banner.
	window.addEventListener("pwa-request-install", () => {
		if (deferredPrompt) {
			const prompt = deferredPrompt;
			prompt.prompt();
			deferredPrompt = null;
			prompt.userChoice
				.then(() => {
					writeFlag(INSTALL_DISMISSED_KEY, true);
					hideInstallBanner();
				})
				.catch(() => {});
		} else {
			showInstallBanner();
		}
	});

	window.addEventListener("appinstalled", () => {
		deferredPrompt = null;
		writeFlag(INSTALL_DISMISSED_KEY, true);
		hideInstallBanner();
		registerPeriodicSync();
	});

	// ── Periodic background sync (task 8) ───────────────────────────────
	// Refresh cached feed data roughly every 6 hours while the installed PWA is
	// idle. Requires the app to be installed and the permission granted; this is
	// best-effort on Chromium and silently skipped elsewhere.
	async function registerPeriodicSync() {
		try {
			const permission =
				navigator.permissions &&
				(await navigator.permissions.query({
					name: "periodic-background-sync",
				}));
			const canRun = !permission || permission.state === "granted";
			if (!canRun) return;

			const reg = await navigator.serviceWorker.ready;
			if (!reg.periodicSync) return; // unsupported
			await reg.periodicSync.register("feed-refresh", {
				minInterval: 6 * 60 * 60 * 1000,
			});
		} catch (err) {
			// Not installed / not permitted / unsupported — nothing to do.
		}
	}

	// Also attempt registration on first load once the SW is ready (covers apps
	// installed before this feature landed); it no-ops unless installable.
	window.addEventListener("load", () => {
		navigator.serviceWorker.ready.then(registerPeriodicSync).catch(() => {});
	});
})();
