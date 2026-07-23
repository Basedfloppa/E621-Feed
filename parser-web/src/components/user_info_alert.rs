use yew::{function_component, html,  Html, Properties, UseStateHandle};

use crate::pages::UserInfo;

#[derive(Properties, PartialEq)]
pub struct InfoAlertProps {
    pub user: UseStateHandle<Option<UserInfo>>,
    pub error: UseStateHandle<Option<String>>,
}

#[function_component(UserInfoAlert)]
pub fn user_info_alert(props: &InfoAlertProps) -> Html {
    html! {
        <>
            {
                if let Some(user) = &*props.user {
                    html! {
                        <div class="alert alert-success mb-3">
                            {"Selected account: "}
                            <strong>{&user.name}</strong>
                            {format!(" (ID: {})", user.id)}
                        </div>
                    }
                } else {
                    html! {}
                }
            }
            {
                if let Some(err) = &*props.error {
                    html! {
                        <div class="alert alert-error mb-3">{err}</div>
                    }
                } else {
                    html! {}
                }
            }
        </>
    }
}
