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
            // ── Navigation ────────────────────────────────────────
            Step {
                id: "nav-tabs".into(),
                title: Some("Navigation tabs".into()),
                text: "Use these tabs to switch between For You (feed), Trending, Search, Favorites, and Digest. The active tab is highlighted.".into(),
                route: Some("/feed".into()),
                attach_to: Some(AttachTo { element: "#header".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Feed / For You ────────────────────────────────────
            Step {
                id: "feed-account".into(),
                title: Some("Select your account on Feed".into()),
                text: "Pick your account from the dropdown here to get personalized recommendations.".into(),
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
                id: "feed-controls".into(),
                title: Some("Feed controls".into()),
                text: "Above the grid you will find score cutoff (Wide/Balanced/Strict), exploration mode (Balanced/Discovery/Focused), grid density, and badge visibility toggles. All settings are saved in your browser.".into(),
                route: Some("/feed".into()),
                attach_to: Some(AttachTo { element: "#feed-account".into(), on: "bottom".into() }),
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
                text: "Global trending posts. With an account selected, you also get a Scored view sorted to your taste profile.".into(),
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
                text: "Search posts by tags. Type a query and get autocomplete suggestions from e621. Results can be viewed raw or scored to your profile.".into(),
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
                text: "Browse your synced favorites from e621. Scored to your taste profile.".into(),
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
                text: "A compact daily summary of top picks based on your taste profile.".into(),
                route: Some("/digest".into()),
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
                title: Some("Settings & quick panel".into()),
                text: "The gear icon in the header opens quick settings (display, filters). The full Settings page has more options like scoring channel weights and A/B experiment mode.".into(),
                route: Some("/settings".into()),
                attach_to: Some(AttachTo { element: "#settings-page".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Account ───────────────────────────────────────────
            Step {
                id: "account-page".into(),
                title: Some("Account setup".into()),
                text: "Add your e621 account here. You will need your account ID and username. Blacklist tags can also be set per-account.".into(),
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
                text: format!("Set per-account blacklist tags here. Leave empty to use the server default. You can copy your e621 blacklist from {domain}/users/[your-id]."),
                route: Some("/account".into()),
                attach_to: Some(AttachTo { element: "#account-blacklist".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Home / Processing ─────────────────────────────────
            Step {
                id: "home-processing".into(),
                title: Some("Build your taste profile".into()),
                text: "On the Home page, select your account and run a Full re-analysis to build your taste profile. The first run fetches all your favorites and builds scoring data. Use Update favourites for routine refreshes.".into(),
                route: Some("/".into()),
                attach_to: Some(AttachTo { element: "#home-account".into(), on: "bottom".into() }),
                buttons: Some(vec![
                    Button { text: "Next".into(), action: "next".into(), classes: None },
                    Button { text: "Back".into(), action: "back".into(), classes: None },
                ]),
                wait_timeout: Some(8000),
                must_be_visible: Some(true),
            },
            // ── Taste Profile ─────────────────────────────────────
            Step {
                id: "taste-profile".into(),
                title: Some("Your taste profile".into()),
                text: "After processing, view your taste profile on the Home page. It shows your favourite tags, artists, characters, and theme clusters discovered from your library.".into(),
                route: Some("/".into()),
                attach_to: None,
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
                text: "That covers the main features. Explore the pages, tweak your settings, and enjoy your personalized feed!".into(),
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
        <div class="drawer">
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
                </ul>
            </div>
        </div>
    }
}
