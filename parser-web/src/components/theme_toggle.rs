use wasm_bindgen::prelude::*;
use web_sys::{wasm_bindgen::prelude::Closure, window, StorageEvent};
use yew::{function_component, html, use_effect_with, use_state, Callback, Html};

#[function_component(ThemeToggle)]
pub fn theme_toggle() -> Html {
    let is_light_theme = use_state(|| {
        let theme = window()
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
            });

        if let Some(doc_elem) = window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        {
            let _ = doc_elem.set_attribute("data-bs-theme", &theme);
        }

        theme == "light"
    });

    use_effect_with((), {
        let is_light_theme = is_light_theme.clone();
        move |_| {
            let handler = Closure::<dyn FnMut(StorageEvent)>::new(move |e: StorageEvent| {
                if e.key().as_deref() != Some("theme") {
                    return;
                }
                
                let theme = e.new_value().unwrap_or_else(|| "light".into());
                let is_light = theme == "light";
                is_light_theme.set(is_light);

                if let Some(doc_elem) = window()
                    .and_then(|w| w.document())
                    .and_then(|d| d.document_element())
                {
                    let _ = doc_elem.set_attribute("data-bs-theme", &theme);
                }
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

    let on_click = {
        let light_theme = is_light_theme.clone();

        Callback::from(move |_| {
            let new_theme = !*light_theme;
            light_theme.set(new_theme);

            let theme_str = if new_theme { "light" } else { "dark" };

            if let Some(doc_elem) = window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
            {
                let _ = doc_elem.set_attribute("data-bs-theme", theme_str);
            }

            if let Some(storage) = window().and_then(|w| w.local_storage().ok()).flatten() {
                let _ = storage.set_item("theme", theme_str);
            }
        })
    };

    let label = if *is_light_theme {
        "Switch to dark theme"
    } else {
        "Switch to light theme"
    };
    html!(
        <button
            type="button"
            class="btn"
            onclick={on_click}
            aria-label={label}
            title={label}
            aria-pressed={(!*is_light_theme).to_string()}
        >
            <i class={ if *is_light_theme {"bi bi-brightness-high"} else {"bi bi-moon"}} aria-hidden="true"></i>
        </button>
    )
}
