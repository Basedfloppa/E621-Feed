//! Shared, humanized error alert with an optional retry action.
//!
//! Centralises the `alert alert-error` markup that used to be duplicated in
//! `post_grid`, `history`, `user_info_alert` and `tag_relation_graph_card`,
//! so every fetch site renders a consistent error state. The message is
//! expected to already be humanized via `humanize_error_body` /
//! `humanize_network_error`.

use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct ErrorAlertProps {
    /// Humanized error message to show.
    pub message: String,
    /// When set, renders a Retry button that re-triggers the fetch.
    #[prop_or_default]
    pub on_retry: Option<Callback<()>>,
    /// Optional custom label for the retry button.
    #[prop_or_else(|| "Retry".to_string())]
    pub retry_label: String,
}

#[function_component(ErrorAlert)]
pub fn error_alert(props: &ErrorAlertProps) -> Html {
    let retry = props
        .on_retry
        .as_ref()
        .map(|cb| cb.reform(|_: MouseEvent| ()));
    html! {
        <div class="alert alert-error mb-3" role="alert" aria-live="polite">
            <span>{ &props.message }</span>
            if let Some(retry) = retry {
                <button
                    type="button"
                    class="btn btn-sm btn-outline"
                    onclick={retry}
                >{ &props.retry_label }</button>
            }
        </div>
    }
}
