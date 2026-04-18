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

pub fn get_or_create_owner_token() -> Option<String> {
    use web_sys::window;

    let storage = window()?.local_storage().ok()??;
    if let Ok(Some(existing)) = storage.get_item("owner_token") {
        if !existing.trim().is_empty() {
            return Some(existing);
        }
    }

    let token = format!(
        "owner-{}-{}-{}",
        js_sys::Date::now() as u64,
        (js_sys::Math::random() * 1_000_000_000.0) as u64,
        (js_sys::Math::random() * 1_000_000_000.0) as u64,
    );

    let _ = storage.set_item("owner_token", &token);
    Some(token)
}
