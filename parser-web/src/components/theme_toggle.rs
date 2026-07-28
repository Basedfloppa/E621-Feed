use wasm_bindgen::prelude::*;
use web_sys::{StorageEvent, wasm_bindgen::prelude::Closure, window};
use yew::{Callback, Html, classes, function_component, html, use_effect_with, use_state};

/// Available themes and their human-readable labels.
const THEMES: &[(&str, &str)] = &[
    ("light", "Light"),
    ("dark", "Dark"),
    ("dim", "Dim"),
    ("nord", "Nord"),
    ("sunset", "Sunset"),
    ("autumn", "Autumn"),
    ("night", "Night"),
    ("coffee", "Coffee"),
    ("winter", "Winter"),
    ("business", "Business"),
    ("lemonade", "Lemon"),
];

fn read_theme() -> String {
    window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("theme").ok())
        .flatten()
        .unwrap_or_else(|| {
            if window()
                .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok())
                .flatten()
                .map(|m| m.matches())
                .unwrap_or(false)
            {
                "dark".to_string()
            } else {
                "light".to_string()
            }
        })
}

fn apply_theme(theme: &str) {
    if let Some(doc_elem) = window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = doc_elem.set_attribute("data-theme", theme);
    }
    if let Some(storage) = window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = storage.set_item("theme", theme);
    }
}

#[function_component(ThemeToggle)]
pub fn theme_toggle() -> Html {
    let current = use_state(|| {
        let t = read_theme();
        apply_theme(&t);
        t
    });

    use_effect_with((), {
        let current = current.clone();
        move |_| {
            let handler = Closure::<dyn FnMut(StorageEvent)>::new(move |e: StorageEvent| {
                if e.key().as_deref() != Some("theme") {
                    return;
                }
                let theme = e.new_value().unwrap_or_else(|| "light".into());
                current.set(theme.clone());
                apply_theme(&theme);
            });

            window()
                .unwrap()
                .add_event_listener_with_callback("storage", handler.as_ref().unchecked_ref())
                .expect("Failed to add storage event listener");

            move || {
                window()
                    .unwrap()
                    .remove_event_listener_with_callback(
                        "storage",
                        handler.as_ref().unchecked_ref(),
                    )
                    .expect("Failed to remove storage event listener");
            }
        }
    });

    let on_select = {
        let current = current.clone();
        Callback::from(move |new_theme: String| {
            current.set(new_theme.clone());
            apply_theme(&new_theme);
        })
    };

    html!(
        <details class="dropdown dropdown-end">
            <summary class="btn btn-ghost btn-sm" aria-label="Select theme">
                <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="1.5" aria-hidden="true" class="inline-block">
                    <path d="M12 2C6.49 2 2 6.49 2 12s4.49 10 10 10c1.1 0 2-.9 2-2 0-.5-.19-.95-.5-1.3-.31-.36-.5-.81-.5-1.3 0-1.1.9-2 2-2H17c3.31 0 6-2.69 6-6 0-4.96-4.49-9-11-9zm0 4.5c-.83 0-1.5-.67-1.5-1.5s.67-1.5 1.5-1.5 1.5.67 1.5 1.5-.67 1.5-1.5 1.5zm-4.5 3c-.83 0-1.5-.67-1.5-1.5s.67-1.5 1.5-1.5 1.5.67 1.5 1.5-.67 1.5-1.5 1.5zm-1.5 4.5c-.83 0-1.5-.67-1.5-1.5s.67-1.5 1.5-1.5 1.5.67 1.5 1.5-.67 1.5-1.5 1.5z"/>
                </svg>
            </summary>
            <ul class="dropdown-content menu p-2 shadow bg-base-100 rounded-box w-32 z-50">
                { for THEMES.iter().map(|(key, label)| {
                    let is_active = *current == *key;
                    let key = key.to_string();
                    let onclick = on_select.reform(move |_: yew::MouseEvent| key.clone());
                    html! {
                        <li>
                            <a
                                class={classes!(if is_active { "menu-active" } else { "" })}
                                onclick={onclick}
                            >
                                { *label }
                            </a>
                        </li>
                    }
                }) }
            </ul>
        </details>
    )
}
