use serde::Serialize;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/static/tour.js")]
extern "C" {
    #[wasm_bindgen(js_name = startTour)]
    fn start_tour_js(steps: &JsValue, options: &JsValue);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachTo {
    pub element: String,
    pub on: String,
}

#[derive(Serialize)]
pub struct Button {
    pub text: String,
    /// "next" | "back" | "cancel"
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_to: Option<AttachTo>,

    // Route-aware extras:
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub must_be_visible: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Button>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TourOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_step_options: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tour_options: Option<serde_json::Value>,
}

pub fn start_tour(steps: Vec<Step>) {
    // Serialising the tour config is not worth a panic: if it fails the app
    // would blank out, so log and skip the tour instead.
    let Ok(steps_js) = serde_wasm_bindgen::to_value(&steps) else {
        web_sys::console::warn_1(&"tour: could not serialise steps; skipping the tour".into());
        return;
    };
    let Ok(opts_js) = serde_wasm_bindgen::to_value(&TourOptions {
        default_step_options: None,
        tour_options: None,
    }) else {
        web_sys::console::warn_1(&"tour: could not serialise options; skipping the tour".into());
        return;
    };
    start_tour_js(&steps_js, &opts_js);
}
