#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::wildcard_imports,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::default_trait_access
)]

use components::*;
use models::bootstrap_session;
use pages::*;
use yew::prelude::*;
use yew_router::prelude::*;

mod components;
mod models;
mod pages;

#[derive(Clone, Routable, PartialEq)]
pub enum Route {
    #[at("/")]
    Home,
    #[at("/account")]
    Account,
    #[at("/feed")]
    Feed,
    #[at("/trending")]
    Trending,
    #[at("/search")]
    Search,
    #[at("/favorites")]
    Favorites,
    #[at("/settings")]
    Settings,
    #[at("/digest")]
    Digest,
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: Route) -> Html {
    match routes {
        Route::Home => html! { <HomePage /> },
        Route::Account => html! { <Account /> },
        Route::Feed => html! { <FeedPage />},
        Route::Trending => html! { <TrendingPage />},
        Route::Search => html! { <SearchPage />},
        Route::Favorites => html! { <FavoritesPage />},
        Route::Settings => html! { <SettingsPage />},
        Route::Digest => html! { <DigestPage />},
        Route::NotFound => html! { <h1>{ "404" }</h1> },
    }
}

#[function_component(App)]
fn app() -> Html {
    html! {
        <BrowserRouter>
            <Header />
            <main id="main-content" class="bg-base-200 min-h-screen pt-4">
                <Switch<Route> render={switch} />
            </main>
        </BrowserRouter>
    }
}

fn main() {
    wasm_bindgen_futures::spawn_local(async {
        bootstrap_session().await;
        yew::Renderer::<App>::new().render();
    });
}
