use components::*;
use models::bootstrap_session;
use pages::*;
use yew::prelude::*;
use yew_router::prelude::*;

mod components;
mod models;
mod pages;

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/")]
    Home,
    #[at("/account")]
    Account,
    #[at("/feed")]
    Feed,
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: Route) -> Html {
    match routes {
        Route::Home => html! { <HomePage /> },
        Route::Account => html! { <Account /> },
        Route::Feed => html! { <FeedPage />},
        Route::NotFound => html! { <h1>{ "404" }</h1> },
    }
}

#[function_component(App)]
fn app() -> Html {
    html! {
        <BrowserRouter>
            <Header />
            <main id="main-content">
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
