//! Themed replacement for `window.confirm()`.
//!
//! Native confirm ignores dark theme, can't be styled, and blocks the
//! UI thread on iOS. This is a Bootstrap modal driven by Yew state —
//! no Bootstrap JS, no modal stack — themed via `data-bs-theme`.
//!
//! Presentational: parents pass `open: bool` plus `on_confirm` /
//! `on_cancel`. Backdrop click cancels.

use yew::{Callback, Children, Html, MouseEvent, Properties, function_component, html};

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
        "btn btn-danger"
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
        <>
            <div
                class="modal fade show d-block"
                tabindex="-1"
                role="dialog"
                aria-modal="true"
                aria-labelledby="confirm-modal-title"
                onclick={on_backdrop_click}
                style="background-color: rgba(0,0,0,0.5);"
            >
                <div class="modal-dialog modal-dialog-centered" role="document" onclick={stop}>
                    <div class="modal-content">
                        <div class="modal-header">
                            <h2 class="modal-title fs-5" id="confirm-modal-title">
                                { props.title.clone() }
                            </h2>
                            <button
                                type="button"
                                class="btn-close"
                                aria-label="Close"
                                onclick={on_cancel.clone()}
                            />
                        </div>
                        <div class="modal-body">
                            { for props.children.iter() }
                        </div>
                        <div class="modal-footer">
                            <button type="button" class="btn btn-secondary" onclick={on_cancel}>
                                { props.cancel_label.clone() }
                            </button>
                            <button type="button" class={confirm_class} onclick={on_confirm}>
                                { props.confirm_label.clone() }
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        </>
    }
}
