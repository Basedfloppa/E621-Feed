use crate::ThemeToggle;
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

    html! {
        <nav class="navbar navbar-expand-lg bg-body-tertiary border sticky-top" id="header">
            <div class="container-fluid d-flex align-items-center gap-2">
                <a class="navbar-brand text-nowrap me-auto" href="/">
                    {"E621 Feed"}
                </a>
                <button
                    type="button"
                    class="btn btn-link nav-link order-lg-3"
                    title="Replay the onboarding tour"
                    aria-label="Replay onboarding tour"
                    onclick={restart_tour}
                >
                    <i class="bi bi-question-circle" aria-hidden="true"></i>
                </button>
                <a
                    class="btn btn-link nav-link order-lg-3"
                    href="https://github.com/Basedfloppa/E621-Feed"
                    target="_blank"
                    rel="noopener noreferrer"
                    title="GitHub repository"
                    aria-label="GitHub repository"
                >
                    <i class="bi bi-github" aria-hidden="true"></i>
                </a>
                <div class="order-lg-3"><ThemeToggle /></div>
                <button
                    class="navbar-toggler order-lg-3"
                    type="button"
                    data-bs-toggle="collapse"
                    data-bs-target="#header-nav-collapse"
                    aria-controls="header-nav-collapse"
                    aria-expanded="false"
                    aria-label="Toggle navigation"
                >
                    <span class="navbar-toggler-icon"></span>
                </button>
                <div class="collapse navbar-collapse order-lg-2" id="header-nav-collapse">
                    <ul class="navbar-nav me-auto gap-1">
                        <li class="nav-item">
                            <a
                                class={classes!("nav-link", is_active("/").then_some("active"))}
                                aria-current={is_active("/").then_some("page")}
                                href="/"
                            >
                                {"Home"}
                            </a>
                        </li>
                        <li class="nav-item">
                            <a
                                class={classes!("nav-link", is_active("/account").then_some("active"))}
                                aria-current={is_active("/account").then_some("page")}
                                href="/account"
                            >
                                {"Account"}
                            </a>
                        </li>
                        <li class="nav-item">
                            <a
                                class={classes!("nav-link", is_active("/feed").then_some("active"))}
                                aria-current={is_active("/feed").then_some("page")}
                                href="/feed"
                            >
                                {"Feed"}
                            </a>
                        </li>
                        <li class="nav-item">
                            <a
                                class={classes!("nav-link", is_active("/digest").then_some("active"))}
                                aria-current={is_active("/digest").then_some("page")}
                                href="/digest"
                            >
                                {"Digest"}
                            </a>
                        </li>
                    </ul>
                </div>
            </div>
        </nav>
    }
}
