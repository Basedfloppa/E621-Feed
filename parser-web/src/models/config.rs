use serde::Deserialize;
use web_sys::js_sys;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub posts_domain: String,
    pub backend_domain: String,
}

pub fn read_config_from_head() -> Option<Config> {
    use wasm_bindgen::JsValue;
    use web_sys::window;

    let w = window()?;
    let v = js_sys::Reflect::get(&w, &JsValue::from_str("APP_CONFIG")).ok()?;
    serde_wasm_bindgen::from_value(v).ok()
}
