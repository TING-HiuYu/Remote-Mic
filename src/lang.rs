//! Simple JSON-based localization loader.
use std::collections::HashMap;
use serde::Deserialize;
use once_cell::sync::OnceCell;
use parking_lot::RwLock;

#[derive(Debug, Deserialize)]
pub struct LangMap(HashMap<String, String>);

impl LangMap {
    /// Fetch key translation or return the key itself if missing.
    pub fn get(&self, key: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| key.to_string())
    }
}

static LANG: OnceCell<RwLock<LangMap>> = OnceCell::new();

// Include the generated embedding table from build.rs
// Provides: pub static EMBEDDED_LANGS: &[(&str, &str)]
include!(concat!(env!("OUT_DIR"), "/lang_data.rs"));

fn parse_embedded(code: &str) -> Option<LangMap> {
    EMBEDDED_LANGS.iter().find(|(c, _)| *c == code).and_then(|(_, raw)| {
        serde_json::from_str::<HashMap<String, String>>(raw).ok().map(LangMap)
    })
}

/// Initialize global language map (one-time). Subsequent calls are ignored.
pub fn init_lang(code: &str) {
    if let Some(map) = parse_embedded(code) { LANG.set(RwLock::new(map)).ok(); }
}

/// Reload (switch) language from embedded table.
pub fn reload_lang(code: &str) {
    if let Some(cell) = LANG.get() { if let Some(map) = parse_embedded(code) { *cell.write() = map; } }
}

/// Translate a key using the active language map (fallback to key).
pub fn tr(key: &str) -> String { LANG.get().map(|l| l.read().get(key)).unwrap_or_else(|| key.to_string()) }

/// List embedded language codes.
pub fn available_langs() -> Vec<String> {
    EMBEDDED_LANGS.iter().map(|(c, _)| (*c).to_string()).collect()
}

/// Fetch the `this.lang` display value from embedded data.
pub fn lang_display(code: &str) -> String {
    if let Some((_, raw)) = EMBEDDED_LANGS.iter().find(|(c, _)| *c == code) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(raw) {
            return map.get("this.lang").cloned().unwrap_or_else(|| code.to_string());
        }
    }
    code.to_string()
}
