use crate::components::{
    IconGithub, IconPerson, IconQuestion, IconSliders, QuickSettingsModal, ThemeToggle,
};
use crate::models::{AttachTo, Button, Step, read_config_from_head, start_tour};
use yew::{
    Callback, Html, MouseEvent, classes, function_component, html, use_effect_with, use_state,
};
use yew_router::prelude::use_location;

fn should_run_tour() -> bool {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return false,
    };
    let storage = match win.local_storage() {
        Ok(Some(s)) => s,
        _ => return false,
    };
    match storage.get_item("finished_tour") {
        Ok(Some(v)) => v == "false",
        Ok(None) => true,
        Err(_) => false,
    }
}

fn mark_tour_finished() {
    if let Some(win) = web_sys::window()
        && let Ok(Some(storage)) = win.local_storage()
    {
        let _ = storage.set_item("finished_tour", "true");
    }
}

#[function_component(Header)]
pub fn header() -> Html {
    // `use_location` re-renders the Header
    let path = {
        let raw = use_location()
            .map(|loc| loc.path().to_string())
            .unwrap_or_else(|| "/".to_string());
        if raw.len() > 1 {
            raw.trim_end_matches('/').to_string()
        } else {
            raw
        }
    };

    let is_active = |p: &str| -> bool {
        if p == "/" {
            path == "/"
        } else {
            let p = p.trim_end_matches('/');
            path == p || path.starts_with(&format!("{p}/"))
        }
    };

    use_effect_with((), |_| {
        if !should_run_tour() {
            return;
        }
        mark_tour_finished();

        let domain = read_config_from_head()
            .map(|c| c.posts_domain)
            .unwrap_or_else(|| "https://e621.net".to_string());
        // ── Tour steps ────────────────────────────────────────────
        // Each step has: id, title, text, route (navigates there),
        // attach_to (element selector + position), buttons.
        // Add/remove steps here; the JS in tour.js handles navigation
        // and element waiting automatically.
        let steps: Vec<Step> = vec![
            Step {
                id: "welcome".into(),
                title: Some("Welcome 👋".into()),
                text: "It looks like you are new here. Would you like a quick tour of the app?".into(),
                route: Some("/".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Yes".into(), action: "next".into(), classes: None },
                    Button { text: "Skip".into(), action: "cancel".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Account (the most important step — start here) ──
            Step {
                id: "account-page".into(),
                title: Some("Add your e621 account".into()),
                text: "This is the most important step. Add your e621 account here (your account ID and username) — that's what lets the server import your favourites and build a personal taste profile. Without an account, nothing on the site is personalized.".into(),
                route: Some("/account".into()),
                attach_to: Some(AttachTo { element: "#account-page".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "account-blacklist".into(),
                title: Some("Account blacklist".into()),
                text: format!("While adding the account you can set its blacklist tags here; leave empty to use the server default. You can copy your full e621 blacklist from {domain}/users/[your-id]."),
                route: Some("/account".into()),
                attach_to: Some(AttachTo { element: "#account-blacklist".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Orientation (on Home) ────────────────────────────
            Step {
                id: "nav-tabs".into(),
                title: Some("Navigation tabs".into()),
                text: "The app is organized into tabs: For You (personalized feed), Trending, Search, Favorites, Digest, and History, plus Home, Account, and Settings. The active tab is highlighted; on small screens they live in the sidebar menu.".into(),
                route: Some("/".into()),
                attach_to: Some(AttachTo { element: "#header".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Home / Build your profile ────────────────────────
            Step {
                id: "home-processing".into(),
                title: Some("Build your taste profile".into()),
                text: "On the Home page, select your account and run a Full re-analysis to build your taste profile. The first run fetches all your favourites and builds scoring data; use Update favourites afterwards for routine refreshes.".into(),
                route: Some("/".into()),
                attach_to: Some(AttachTo { element: "#home-account".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "taste-profile".into(),
                title: Some("Your taste profile".into()),
                text: "After processing, view your taste profile on the Home page — your favourite tags, artists, characters, and theme clusters discovered from your library. This is what drives every personalized page.".into(),
                route: Some("/".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── For You / Feed ───────────────────────────────────
            Step {
                id: "feed".into(),
                title: Some("For You — personalized feed".into()),
                text: "The For You tab scores recent posts against your taste profile and surfaces the ones you'll most likely enjoy. Each post shows a \"Why this post?\" breakdown explaining the picks. This is your main feed.".into(),
                route: Some("/feed".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "feed-account".into(),
                title: Some("Pick your account".into()),
                text: "Choose your account here so the feed knows whose taste profile to use. Without an account selected there's no personalization — just the unranked feed.".into(),
                route: Some("/feed".into()),
                attach_to: Some(AttachTo { element: "#feed-account".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "feed-exploration".into(),
                title: Some("Feed controls".into()),
                text: "Above the grid: Exploration (Focused / Balanced / Discovery) balances showing your top picks against surfacing novel content. Grid density, badges, and the score cutoff now live under Settings → Display.".into(),
                route: Some("/feed".into()),
                attach_to: Some(AttachTo { element: "#feed-exploration".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Trending ──────────────────────────────────────────
            Step {
                id: "trending".into(),
                title: Some("Trending page".into()),
                text: "Global trending posts, independent of your profile. With an account selected you can switch to a Scored view that re-ranks them by your taste — see how the site's popularity compares to your preferences.".into(),
                route: Some("/trending".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Search ────────────────────────────────────────────
            Step {
                id: "search".into(),
                title: Some("Search & tag autocomplete".into()),
                text: "Search posts by tags — type a query and get autocomplete suggestions from e621. Results can be viewed raw, or scored to your profile to surface the matching posts you'd like most.".into(),
                route: Some("/search".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Favorites ─────────────────────────────────────────
            Step {
                id: "favorites".into(),
                title: Some("Favorites browser".into()),
                text: "Browse the favourites you've imported from e621, scored to your taste profile. Useful for reviewing what's driving your recommendations.".into(),
                route: Some("/favorites".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Digest ────────────────────────────────────────────
            Step {
                id: "digest".into(),
                title: Some("Daily digest".into()),
                text: "A compact daily digest: Quick mode serves stratified picks (top, trending, exploration, wildcard, recent) fast; Full mode scores the whole catalog against your profile but takes longer. Either way, open a card's \"Why this post?\" breakdown to see the reasoning.".into(),
                route: Some("/digest".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── History ───────────────────────────────────────────
            Step {
                id: "history".into(),
                title: Some("Interaction history".into()),
                text: "Everything you've opened, liked, or hidden, filterable by event (All / Open / Like / Strong like / Hide). It's the feedback your profile was built on, so you can review or audit it.".into(),
                route: Some("/history".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Settings ──────────────────────────────────────────
            Step {
                id: "settings".into(),
                title: Some("Settings page".into()),
                text: "Settings is split into per-account Server settings (blacklist, preferred tags), Display settings (score results, cutoff, grid density, badges — saved in your browser), and a Storage / Offline section for installing the app and clearing cached data.".into(),
                route: Some("/settings".into()),
                attach_to: Some(AttachTo { element: "#settings-page".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "settings-display".into(),
                title: Some("Display settings".into()),
                text: "Saved in your browser and applied everywhere: turn on \"Score results\" to rank by your profile and pick a cutoff (Wide/Balanced/Strict), choose grid density, and toggle which badges and card details (rating, tags, breakdowns) to show. A live Preview updates as you change them.".into(),
                route: Some("/settings".into()),
                attach_to: Some(AttachTo { element: "#settings-display".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            Step {
                id: "settings-storage".into(),
                title: Some("Storage / Offline".into()),
                text: "The app caches data on your device (via a service worker) so pages keep working offline. Here you can see how much space is used, clear the offline cache, and control the \"install as an app\" prompt (or trigger a native install).".into(),
                route: Some("/settings".into()),
                attach_to: Some(AttachTo { element: "#settings-storage".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Final ─────────────────────────────────────────────
            Step {
                id: "final".into(),
                title: Some("You are all set 🎉".into()),
                text: "That covers the main features. Start by adding your account, run a re-analysis, and enjoy your personalized feed!".into(),
                route: Some("/".into()),
                attach_to: None,
                buttons: Some(vec![
                    Button { text: "Finish".into(), action: "cancel".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
        ];
        start_tour(steps);
    });

    let restart_tour = Callback::from(|e: MouseEvent| {
        e.prevent_default();
        if let Some(win) = web_sys::window() {
            if let Some(store) = win.local_storage().ok().flatten() {
                let _ = store.set_item("finished_tour", "false");
            }
            let _ = win.location().reload();
        }
    });

    let account_active = is_active("/account");
    let quick_settings_open = use_state(|| false);
    let open_quick_settings = {
        let quick_settings_open = quick_settings_open.clone();
        Callback::from(move |_| quick_settings_open.set(true))
    };
    let close_quick_settings = {
        let quick_settings_open = quick_settings_open.clone();
        Callback::from(move |_| quick_settings_open.set(false))
    };

    html! {
        <div class="drawer sticky top-0 z-40">
            <input id="header-drawer" type="checkbox" class="drawer-toggle" aria-label="Toggle navigation" />
            <div class="drawer-content flex flex-col">
                <nav class="navbar bg-base-100 border-b border-base-300 sticky top-0 shadow-md z-10" id="header">
                    <div class="navbar-start">
                        <label
                            for="header-drawer"
                            class="btn btn-ghost drawer-button lg:hidden"
                            aria-label="Open navigation menu"
                        >
                            <svg xmlns="http://www.w3.org/2000/svg" class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 6h16M4 12h16M4 18h16" />
                            </svg>
                        </label>
                        <a class="btn btn-ghost text-xl" href="/">
                            {"E621 Feed"}
                        </a>
                    </div>
                    <div class="navbar-center hidden lg:flex">
                        <div role="tablist" class="tabs tabs-box">
                            <a role="tab"
                                class={classes!("tab", is_active("/feed").then_some("tab-active"))}
                                href="/feed"
                            >
                                {"For You"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/trending").then_some("tab-active"))}
                                href="/trending"
                            >
                                {"Trending"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/search").then_some("tab-active"))}
                                href="/search"
                            >
                                {"Search"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/favorites").then_some("tab-active"))}
                                href="/favorites"
                            >
                                {"Favorites"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/digest").then_some("tab-active"))}
                                href="/digest"
                            >
                                {"Digest"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/history").then_some("tab-active"))}
                                href="/history"
                            >
                                {"History"}
                            </a>
                            <a role="tab"
                                class={classes!("tab", is_active("/catalog").then_some("tab-active"))}
                                href="/catalog"
                            >
                                {"Catalog"}
                            </a>
                        </div>
                    </div>
                    <div class="navbar-end gap-1">
                        <button
                            type="button"
                            class="btn btn-ghost btn-sm"
                            aria-label="Quick settings"
                            title="Quick settings"
                            onclick={open_quick_settings}
                        >
                            <IconSliders />
                        </button>
                        <a
                            class={classes!("btn", "btn-ghost", "btn-sm", account_active.then_some("btn-active"))}
                            aria-label="Account settings"
                            title="Account settings"
                            href="/account"
                        >
                            <IconPerson active={account_active} />
                        </a>
                        <button
                            type="button"
                            class="btn btn-ghost btn-sm"
                            title="Replay the onboarding tour"
                            aria-label="Replay onboarding tour"
                            onclick={restart_tour}
                        >
                            <IconQuestion />
                        </button>
                        <a
                            class="btn btn-ghost btn-sm"
                            href="https://github.com/Basedfloppa/E621-Feed"
                            target="_blank"
                            rel="noopener noreferrer"
                            title="GitHub repository"
                            aria-label="GitHub repository"
                        >
                            <IconGithub />
                        </a>
                        <ThemeToggle />
                    </div>
                </nav>
            </div>
            <QuickSettingsModal open={*quick_settings_open} on_close={close_quick_settings} />
            <div class="drawer-side z-50">
                <label for="header-drawer" aria-label="close sidebar" class="drawer-overlay"></label>
                <ul class="menu p-4 w-80 min-h-full bg-base-200">
                    <li class="menu-title"><span>{"Navigation"}</span></li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/").then_some("menu-active"))}
                            href="/"
                        >
                            {"Home"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/account").then_some("menu-active"))}
                            href="/account"
                        >
                            {"Account"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/feed").then_some("menu-active"))}
                            href="/feed"
                        >
                            {"For You"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/trending").then_some("menu-active"))}
                            href="/trending"
                        >
                            {"Trending"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/search").then_some("menu-active"))}
                            href="/search"
                        >
                            {"Search"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/favorites").then_some("menu-active"))}
                            href="/favorites"
                        >
                            {"Favorites"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/settings").then_some("menu-active"))}
                            href="/settings"
                        >
                            {"Settings"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/digest").then_some("menu-active"))}
                            href="/digest"
                        >
                            {"Digest"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/history").then_some("menu-active"))}
                            href="/history"
                        >
                            {"History"}
                        </a>
                    </li>
                    <li>
                        <a
                            class={classes!("text-base-content", is_active("/catalog").then_some("menu-active"))}
                            href="/catalog"
                        >
                            {"Catalog"}
                        </a>
                    </li>
                </ul>
            </div>
        </div>
    }
}
