import Shepherd from '/static/vendor/shepherd.mjs';

let TOUR = null;

function normalizeButtons(tour, buttons = []) {
    return buttons.map(b => {
        const action =
            typeof b.action === 'string'
                ? (b.action === 'next' ? tour.next
                    : b.action === 'back' ? tour.back
                        : tour.cancel)
                : b.action;
        return { ...b, action };
    });
}

function waitForSelector(selector, { timeout = 8000, mustBeVisible = true } = {}) {
    const start = performance.now();

    return new Promise((resolve, reject) => {
        function isVisible(el) {
            if (!el) return false;
            const rect = el.getBoundingClientRect();
            return rect.width > 0 && rect.height > 0;
        }

        function check() {
            const el = document.querySelector(selector);
            if (el && (!mustBeVisible || isVisible(el))) {
                resolve(el);
                return;
            }
            if (performance.now() - start > timeout) {
                reject(new Error(`Timeout waiting for ${selector}`));
                return;
            }
            requestAnimationFrame(check);
        }
        check();
    });
}

function navigateTo(path) {
    if (path && location.pathname !== path) {
        history.pushState({}, "", path);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }
}

function buildStep(tour, raw) {
    const step = { ...raw };

    if (Array.isArray(step.buttons)) {
        step.buttons = normalizeButtons(tour, step.buttons);
    }

    const wantRoute = raw.route;
    const attach = raw.attachTo?.element;

    step.beforeShowPromise = () => {
        // 1) Navigate if needed
        if (wantRoute && location.pathname !== wantRoute) {
            navigateTo(wantRoute);
        }

        // 2) If step attaches to an element, wait for it to exist/visible
        if (attach) {
            const timeout = raw.waitTimeout ?? 8000;
            const mustBeVisible = raw.mustBeVisible ?? true;
            return waitForSelector(attach, { timeout, mustBeVisible }).then(() => {
                const el = document.querySelector(attach);
                if (el) el.scrollIntoView({ block: "center", behavior: "smooth" });
            });
        }
        return Promise.resolve();
    };

    return step;
}

function attachFocusTrap(tour) {
    let prevFocus = null;
    let trapHandler = null;

    function focusableNodes(stepEl) {
        return Array.from(stepEl.querySelectorAll(
            'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
        ));
    }

    function detach() {
        if (trapHandler) {
            document.removeEventListener('keydown', trapHandler, true);
            trapHandler = null;
        }
    }

    function restorePrevFocus() {
        if (prevFocus && typeof prevFocus.focus === 'function') {
            try { prevFocus.focus(); } catch (_) { /* element may have been removed */ }
        }
        prevFocus = null;
    }

    tour.on('show', () => {
        const stepEl = tour.getCurrentStep()?.getElement();
        if (!stepEl) return;
        if (!prevFocus) {
            prevFocus = document.activeElement;
        }

        // Detach any previous step's handler so we don't stack listeners.
        detach();

        const focusables = focusableNodes(stepEl);
        if (focusables.length > 0) {
            // Defer to the next frame: Shepherd inserts the step element
            // and animates it in; focusing too early loses to the
            // animation reset.
            requestAnimationFrame(() => focusables[0].focus());
        }

        trapHandler = (e) => {
            if (e.key === 'Escape') {
                e.preventDefault();
                tour.cancel();
                return;
            }
            if (e.key !== 'Tab') return;
            const live = focusableNodes(stepEl);
            if (live.length === 0) {
                e.preventDefault();
                return;
            }
            const first = live[0];
            const last = live[live.length - 1];
            if (e.shiftKey && document.activeElement === first) {
                e.preventDefault();
                last.focus();
            } else if (!e.shiftKey && document.activeElement === last) {
                e.preventDefault();
                first.focus();
            }
        };
        document.addEventListener('keydown', trapHandler, true);
    });

    tour.on('hide', detach);
    tour.on('complete', () => { detach(); restorePrevFocus(); });
    tour.on('cancel', () => { detach(); restorePrevFocus(); });
}

export function startTour(steps = [], options = {}) {
    if (TOUR) {
        TOUR.cancel();
        TOUR = null;
    }

    TOUR = new Shepherd.Tour({
        useModalOverlay: true,
        defaultStepOptions: {
            cancelIcon: { enabled: true },
            scrollTo: false,
            ...options.defaultStepOptions
        },
        ...options.tourOptions
    });

    steps.forEach(raw => TOUR.addStep(buildStep(TOUR, raw)));
    attachFocusTrap(TOUR);
    TOUR.start();
}

export function resumeTour() {
    if (TOUR) TOUR.start();
}

export function cancelTour() {
    if (TOUR) TOUR.cancel();
}

export function isRunning() {
    return !!TOUR && TOUR.isActive();
}
