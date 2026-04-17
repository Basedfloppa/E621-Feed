use crate::ThemeToggle;
use yew::{Html, classes, function_component, html};
use crate::models::{read_config_from_head, start_tour, AttachTo, Button, Step};

#[function_component(Header)]
pub fn header() -> Html {
    let path = {
        let p = if let Some(win) = web_sys::window() {
            win.location()
                .pathname()
                .unwrap_or_else(|_| "/".to_string())
        } else {
            "/".to_string()
        };
        if p.len() > 1 {
            p.trim_end_matches('/').to_string()
        } else {
            p
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

    {
        let window = web_sys::window().unwrap();
        let local_storage = window.local_storage().unwrap().expect("");
        let site_tour = local_storage.get_item("finished_tour").unwrap();
        if site_tour.is_none() || site_tour.unwrap() == "false" {
            let domain = read_config_from_head().unwrap().posts_domain;
            let steps = vec![
                Step {
                    id: "welcome".into(),
                    title: Some("Welcome 👋".into()),
                    text: "It seems like you are new here. Would you like to get the website tour?".into(),
                    route: Some("/".into()),
                    attach_to: Some(AttachTo { element: "#header".into(), on: "bottom".into() }),
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
                    title: Some("Account id.".into()),
                    text: format!("This is a field for your account id, you can find it in url when opening your profile in {domain}. {domain}/users/[your-id-here]"),
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
                    text: format!("This is a field for your account blacklist, you can leave it empty if you are content with default blacklist, if you want to add your own, copy it from {domain}."),
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
                    text: "After you added your account, you can pick it from selectors.".into(),
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
                    title: Some("Tag analyzer.".into()),
                    text: "After you picked your account, you can start tag analyzer, it will scan your favourites posts for tags and build statistics that will be used for post recommendations.".into(),
                    route: Some("/".into()),
                    attach_to: Some(AttachTo { element: "#home-analyzer".into(), on: "bottom".into() }),
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
                    text: "After you finished with tag analysis, you can go to feed page and pick your account from selector.".into(),
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
                    title: Some("Post affinity.".into()),
                    text: "But before that you should pick post affinity, so you dont see things you may not like, i recomment to set it to 0.2 for the starters.".into(),
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
                    title: Some("Feed grid.".into()),
                    text: "Here you can pick grid layout for more comfort viewing of post on you screen width.".into(),
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
                    text: "And that it, hope you'll have a good time using the site!".into(),
                    route: Some("/".into()),
                    attach_to: Some(AttachTo { element: "#header".into(), on: "bottom".into() }),
                    buttons: Some(vec![
                        Button { text: "Finish".into(), action: "done".into(), classes: None },
                        Button { text: "Back".into(), action: "back".into(), classes: None },
                    ]),
                    wait_timeout: Some(8000),
                    must_be_visible: Some(true),
                },
            ];
            start_tour(steps);
            let _ = local_storage.set_item("finished_tour","true");
        }
    }

    html! {
        <nav class="navbar bg-body-tertiary border" id="header">
            <div class="container-fluid d-flex align-items-center gap-3 flex-wrap">
                <a class="navbar-brand text-nowrap" href="/">
                    {"e621 Account parser"}
                </a>
                <ul class="navbar-nav flex-row me-auto gap-2 flex-wrap">
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
                </ul>
                <ul class="navbar-nav flex-row ms-auto flex-wrap">
                    <li class="nav-item">
                        <ThemeToggle />
                    </li>
                </ul>
            </div>
        </nav>
    }
}
