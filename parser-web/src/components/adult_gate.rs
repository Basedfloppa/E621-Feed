//! "Adult content" gate shown before first entry to the site.
//!
//! Renders a full-screen 18+ warning overlay on first visit. Once the user
//! confirms they are an adult, the choice is remembered in localStorage
//! (`adult_gate_accepted`) so the gate doesn't nag on every load.

use yew::{Callback, Html, MouseEvent, function_component, html, use_state};

const ACCEPTED_KEY: &str = "adult_gate_accepted";

fn read_flag(key: &str, def: bool) -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item(key).ok())
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(def)
}

fn write_flag(key: &str, val: bool) {
    if let Some(s) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    {
        let _ = s.set_item(key, if val { "1" } else { "0" });
    }
}

#[function_component(AdultGate)]
pub fn adult_gate() -> Html {
    let accepted = use_state(|| read_flag(ACCEPTED_KEY, false));

    let accept = {
        let accepted = accepted.clone();
        Callback::from(move |_: MouseEvent| {
            write_flag(ACCEPTED_KEY, true);
            accepted.set(true);
        })
    };

    let leave = Callback::from(|_: MouseEvent| {
        if let Some(w) = web_sys::window() {
            let _ = w.location().assign("https://e621.net");
        }
    });

    if *accepted {
        return Html::default();
    }

    html! {
        <div class="fixed inset-0 z-[10000] flex items-center justify-center p-4 bg-base-100">
            <div class="card bg-base-200 border border-base-300 shadow-xl max-w-lg w-full">
                <div class="card-body items-center text-center">
                    <div class="text-5xl my-2" aria-hidden="true">{"🔞"}</div>
                    <h1 class="card-title text-2xl justify-center">{ "Adult content — 18+" }</h1>
                    <p class="text-sm text-base-content/80">
                        { "This is a personalised recommendation feed for E621 — an adult image board. Posts may contain explicit sexual 18+ content." }
                    </p>
                    <p class="text-sm text-base-content/70">
                        { "By continuing, you confirm you are at least 18 years old and that viewing adult material is legal in your place of residence." }
                    </p>
                    <div class="flex flex-wrap gap-2 justify-center mt-2">
                        <button type="button" class="btn btn-primary" onclick={accept}>
                            { "I am 18+ — Enter the site" }
                        </button>
                        <button type="button" class="btn btn-ghost" onclick={leave}>
                            { "Leave this site" }
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}
