use crate::components::{IconGithub, IconPerson, IconQuestion, ThemeToggle};
use yew::{Callback, Html, MouseEvent, classes, function_component, html, use_effect_with};
use yew_router::prelude::use_location;
use crate::models::{read_config_from_head, start_tour, AttachTo, Button, Step};

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
        && let Ok(Some(storage)) = win.local_storage() {
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
        let steps = vec![
                Step {
                    id: "welcome".into(),
                    title: Some("Welcome 👋".into()),
                    text: "It seems like you are new here. Would you like to get the website tour?".into(),
                    route: Some("/".into()),
                    attach_to: None,
                    buttons: Some(vec![
                        Button { text: "Yes".into(), action: "next".into(), classes: None },
                        Button { text: "Skip".into(), action: "cancel".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "account-page".into(),
                    title: Some("Account page.".into()),
                    text: "Your account data lives here.".into(),
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
                    id: "account-id".into(),
                    title: Some("Account ID.".into()),
                    text: format!("This is your account ID field — you can find it in the URL when opening your profile: {domain}/users/[your-id-here]"),
                    route: Some("/account".into()),
                    attach_to: Some(AttachTo { element: "#account-id".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Next".into(), action: "next".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "account-username".into(),
                    title: Some("Account username.".into()),
                    text: "This is a field for your account username.".into(),
                    route: Some("/account".into()),
                    attach_to: Some(AttachTo { element: "#account-name".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Next".into(), action: "next".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "account-blacklist".into(),
                    title: Some("Account blacklist.".into()),
                    text: format!("This is a field for your account blacklist. Leave it empty to use the default blacklist, or copy your own from {domain}."),
                    route: Some("/account".into()),
                    attach_to: Some(AttachTo { element: "#account-blacklist".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Next".into(), action: "next".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "home-account".into(),
                    title: Some("Pick your account.".into()),
                    text: "Once you've added your account, you can pick it from the selectors.".into(),
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
                    id: "home-analyzer".into(),
                    title: Some("Build your taste profile.".into()),
                    text: "Run a full analysis the first time. It requests every expected favourites page, replaces the stored favourite links, and rebuilds the tag profile, including reconciliation of favourites you removed on e621.".into(),
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
                    id: "home-updates".into(),
                    title: Some("Keep it fresh without a full rebuild.".into()),
                    text: "For routine refreshes, use Update favourites: it fetches only newer favourites and skips the expensive teardown, saving substantial time on large accounts. Incremental updates cannot detect favourites you removed on e621, so run Full re-analysis when you need deletions reconciled.".into(),
                    route: Some("/".into()),
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
                    title: Some("Feed account.".into()),
                    text: "Once tag analysis finishes, head to the feed page and pick your account from the selector.".into(),
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
                    id: "feed-affinity".into(),
                    title: Some("Control each page's cutoff.".into()),
                    text: "This power-user control drops the weakest percentage from each scored page: Wide keeps everything, Balanced drops the bottom 30%, and Strict drops the bottom 60%. It is a relative display filter, not an absolute affinity threshold, and your choice is saved in this browser.".into(),
                    route: Some("/feed".into()),
                    attach_to: Some(AttachTo { element: "#feed-affinity".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Next".into(), action: "next".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "feed-grid".into(),
                    title: Some("Choose your feed density.".into()),
                    text: "Use Auto for a responsive grid, or lock the feed to three, two, or one column. This changes layout only — not recommendation scores — and is saved in this browser.".into(),
                    route: Some("/feed".into()),
                    attach_to: Some(AttachTo { element: "#feed-grid".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Next".into(), action: "next".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
                Step {
                    id: "final".into(),
                    title: Some("Finally.".into()),
                    text: "And that's it — hope you'll have a good time using the site!".into(),
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
                        </div>
                    </div>
                    <div class="navbar-end gap-1">
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
                <div class="btm-nav btm-nav-sm lg:hidden border-t border-base-300 z-10">
                    <a
                        class={classes!(is_active("/feed").then_some("active"))}
                        href="/feed"
                    >
                        <svg xmlns="http://www.w3.org/2000/svg" class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 6h16M4 10h16M4 14h16M4 18h16" />
                        </svg>
                        <span>{"For You"}</span>
                    </a>
                    <a
                        class={classes!(is_active("/trending").then_some("active"))}
                        href="/trending"
                    >
                        <svg xmlns="http://www.w3.org/2000/svg" class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7h8m0 0v8m0-8l-8 8-4-4-6 6" />
                        </svg>
                        <span>{"Trending"}</span>
                    </a>
                    <a
                        class={classes!(is_active("/favorites").then_some("active"))}
                        href="/favorites"
                    >
                        <svg xmlns="http://www.w3.org/2000/svg" class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4.318 6.318a4.5 4.5 0 000 6.364L12 20.364l7.682-7.682a4.5 4.5 0 00-6.364-6.364L12 7.636l-1.318-1.318a4.5 4.5 0 00-6.364 0z" />
                        </svg>
                        <span>{"Favorites"}</span>
                    </a>
                    <a
                        class={classes!(is_active("/digest").then_some("active"))}
                        href="/digest"
                    >
                        <svg xmlns="http://www.w3.org/2000/svg" class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                        </svg>
                        <span>{"Digest"}</span>
                    </a>
                </div>
            </div>
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
                            class={classes!("text-base-content", is_active("/favorites").then_some("menu-active"))}
                            href="/favorites"
                        >
                            {"Favorites"}
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
                </ul>
            </div>
        </div>
    }
}
