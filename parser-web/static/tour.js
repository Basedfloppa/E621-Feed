import Shepherd from './vendor/shepherd.mjs';

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
