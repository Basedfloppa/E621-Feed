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

    let token = mint_csprng_token()?;
    let _ = storage.set_item("owner_token", &token);
    Some(token)
}

fn mint_csprng_token() -> Option<String> {
    use web_sys::window;

    let crypto = window()?.crypto().ok()?;
    let mut bytes = [0u8; 32];
    crypto.get_random_values_with_u8_array(&mut bytes).ok()?;
    Some(base64url_encode(&bytes))
}

/// Compact RFC 4648 §5 (URL-safe) base64 without padding. Pulling in a
/// full `base64` crate just for one 32-byte call would be silly; the
/// alphabet difference from standard base64 is just `+/` → `-_`.
fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = u32::from(bytes[i]) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}
