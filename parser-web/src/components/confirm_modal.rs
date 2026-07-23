//! Themed replacement for `window.confirm()`.
//!
//! Native confirm ignores dark theme, can't be styled, and blocks the
//! UI thread on iOS. This is a DaisyUI `<dialog>` modal driven by Yew
//! state — no JS, themed via `data-theme`.
//!
//! Presentational: parents pass `open: bool` plus `on_confirm` /
//! `on_cancel`. Backdrop click cancels.

use yew::{function_component, html, Callback, Children, Html, MouseEvent, Properties};

#[derive(Properties, PartialEq)]
pub struct ConfirmModalProps {
    pub open: bool,
    pub title: String,
    #[prop_or_default]
    pub children: Children,
    #[prop_or_else(|| "Confirm".to_string())]
    pub confirm_label: String,
    #[prop_or_else(|| "Cancel".to_string())]
    pub cancel_label: String,
    #[prop_or(false)]
    pub destructive: bool,
    pub on_confirm: Callback<()>,
    pub on_cancel: Callback<()>,
}

#[function_component(ConfirmModal)]
pub fn confirm_modal(props: &ConfirmModalProps) -> Html {
    if !props.open {
        return html! {};
    }

    let confirm_class = if props.destructive {
        "btn btn-error"
    } else {
        "btn btn-primary"
    };

    let on_backdrop_click = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_e: MouseEvent| on_cancel.emit(()))
    };
    let stop = Callback::from(|e: MouseEvent| e.stop_propagation());
    let on_confirm = {
        let cb = props.on_confirm.clone();
        Callback::from(move |_| cb.emit(()))
    };
    let on_cancel = {
        let cb = props.on_cancel.clone();
        Callback::from(move |_| cb.emit(()))
    };

    html! {
        <dialog
            class="modal modal-open"
            aria-modal="true"
            aria-labelledby="confirm-modal-title"
            onclick={on_backdrop_click}
        >
            <div class="modal-box" onclick={stop}>
                <button
                    type="button"
                    class="btn btn-sm btn-circle btn-ghost absolute right-2 top-2"
                    aria-label="Close"
                    onclick={on_cancel.clone()}
                >
                    { "✕" }
                </button>
                <h2 class="font-bold text-lg text-base-content" id="confirm-modal-title">
                    { props.title.clone() }
                </h2>
                <div class="py-4">
                    { for props.children.iter() }
                </div>
                <div class="modal-action">
                    <button type="button" class="btn btn-ghost" onclick={on_cancel}>
                        { props.cancel_label.clone() }
                    </button>
                    <button type="button" class={confirm_class} onclick={on_confirm}>
                        { props.confirm_label.clone() }
                    </button>
                </div>
            </div>
        </dialog>
    }
}
