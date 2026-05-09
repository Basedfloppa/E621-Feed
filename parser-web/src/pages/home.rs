use serde::{Deserialize, Serialize};
use yew::prelude::*;

use crate::components::*;
use crate::models::read_config_from_head;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TagCount {
    pub name: String,
    pub group_type: String,
    pub count: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserInfo {
    pub id: i64,
    pub name: String,
    pub blacklist: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagView {
    Chart,
    Graph,
}

#[function_component(HomePage)]
pub fn home_page() -> Html {
    let cfg = match read_config_from_head() {
        Some(c) => c,
        None => {
            return html! {
                <div class="container mt-4">
                    <div class="alert alert-danger" role="alert">
                        { "App configuration failed to load. Please reload the page; if the problem persists, check that /static/config.js is reachable." }
                    </div>
                </div>
            };
        }
    };
    let selected_user: UseStateHandle<Option<UserInfo>> = use_state(|| None::<UserInfo>);
    let is_loading: UseStateHandle<bool> = use_state(|| false);
    let tag_counts: UseStateHandle<Vec<TagCount>> = use_state(Vec::<TagCount>::new);
    let error: UseStateHandle<Option<String>> = use_state(|| None::<String>);
    let canvas_ref = use_node_ref();
    let active_view: UseStateHandle<TagView> = use_state(|| TagView::Chart);

    html! {
        <div>
            <div class="container mt-4">
                <div class="row justify-content-center">
                    <div class="col-lg-8">
                        <div class="card shadow-sm">
                            <div class="card-body">
                                <h1 class="card-title text-center mb-4">{"e621 Tag Analyzer"}</h1>

                                <div id="home-account">
                                    <SavedAccountsSelect
                                        selected_user={selected_user.clone()}
                                        is_loading={is_loading.clone()}
                                    />

                                    <UserSearchForm
                                        found_user={selected_user.clone()}
                                        error={error.clone()}
                                        api_base={cfg.backend_domain.clone()}
                                        is_loading={is_loading.clone()}
                                    />
                                </div>

                                <UserInfoAlert
                                    user={selected_user.clone()}
                                    error={error.clone()}
                                />

                                <div id="home-analyzer">
                                    <FetchAnalyzeButton
                                        tag_count={tag_counts.clone()}
                                        found_user={selected_user.clone()}
                                        error={error.clone()}
                                        api_base={cfg.backend_domain.clone()}
                                        is_loading={is_loading.clone()}
                                    />
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
            {
                if selected_user.is_some() {
                    let chart_active = matches!(*active_view, TagView::Chart);
                    let graph_active = matches!(*active_view, TagView::Graph);
                    let on_chart = {
                        let active_view = active_view.clone();
                        Callback::from(move |_| active_view.set(TagView::Chart))
                    };
                    let on_graph = {
                        let active_view = active_view.clone();
                        Callback::from(move |_| active_view.set(TagView::Graph))
                    };
                    html! {
                        <div class="container mt-3">
                            <div class="d-flex justify-content-center">
                                <div class="btn-group" role="group" aria-label="Tag visualisation switcher">
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline-primary", chart_active.then_some("active"))}
                                        aria-pressed={chart_active.to_string()}
                                        onclick={on_chart}
                                    >
                                        { "Bar chart" }
                                    </button>
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline-primary", graph_active.then_some("active"))}
                                        aria-pressed={graph_active.to_string()}
                                        onclick={on_graph}
                                    >
                                        { "Relation graph" }
                                    </button>
                                </div>
                            </div>
                        </div>
                    }
                } else {
                    html! {}
                }
            }
            <div class="container-fluid mt-3 px-3 px-md-4">
                {
                    match *active_view {
                        TagView::Chart => html! {
                            <TagChartCard
                                canvas_ref={canvas_ref.clone()}
                                tag_counts={tag_counts.clone()}
                            />
                        },
                        TagView::Graph => html! {
                            <TagRelationGraphCard
                                found_user={selected_user.clone()}
                                api_base={cfg.backend_domain.clone()}
                            />
                        },
                    }
                }
            </div>
        </div>
    }
}
