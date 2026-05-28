//! User-facing settings for the in-editor browser tab.
//!
//! The `BrowserSettingsContent` schema lives in `settings_content` so the
//! settings UI and JSON schema generation pick it up. This file is just
//! the consumer-side accessor, registered in `browser_viewer::init`.

use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Debug, Clone, RegisterSetting)]
pub struct BrowserSettings {
    pub homepage: String,
    pub search_url: String,
}

impl Settings for BrowserSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let browser = content.browser.as_ref().unwrap();
        Self {
            homepage: browser.homepage.clone().unwrap(),
            search_url: browser.search_url.clone().unwrap(),
        }
    }
}
