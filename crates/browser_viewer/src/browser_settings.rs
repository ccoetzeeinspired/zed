//! User-facing settings for the in-editor browser tab.
//!
//! The `BrowserSettingsContent` schema lives in `settings_content` so the
//! settings UI and JSON schema generation pick it up. This file is just
//! the consumer-side accessor, registered in `browser_viewer::init`.

use std::collections::HashMap;

use settings::{RegisterSetting, Settings, SettingsContent};

/// Credential map key for the login email field (`browser: automation type email`).
pub const AUTOMATION_CRED_EMAIL: &str = "email";
/// Credential map key for the login password field (`browser: automation type password`).
pub const AUTOMATION_CRED_PASSWORD: &str = "password";

#[derive(Debug, Clone, RegisterSetting)]
pub struct BrowserSettings {
    pub homepage: String,
    pub search_url: String,
    /// Dogfood login secrets — keys like `email`, `password`. User settings only.
    pub automation_credentials: HashMap<String, String>,
    pub automation_dev_wait_text: Option<String>,
}

impl BrowserSettings {
    pub fn automation_credential(&self, key: &str) -> Option<String> {
        self.automation_credentials
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
    }

    pub fn automation_email(&self) -> Option<String> {
        self.automation_credential(AUTOMATION_CRED_EMAIL)
    }

    pub fn automation_password(&self) -> Option<String> {
        self.automation_credential(AUTOMATION_CRED_PASSWORD)
    }
}

impl Settings for BrowserSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let browser = content.browser.as_ref().unwrap();
        let mut automation_credentials: HashMap<String, String> = browser
            .automation_credentials
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        // Legacy single-field password → map entry.
        if !automation_credentials.contains_key(AUTOMATION_CRED_PASSWORD) {
            if let Some(password) = browser.automation_dev_password.clone().filter(|p| !p.is_empty())
            {
                automation_credentials.insert(AUTOMATION_CRED_PASSWORD.to_string(), password);
            }
        }
        Self {
            homepage: browser.homepage.clone().unwrap(),
            search_url: browser.search_url.clone().unwrap(),
            automation_credentials,
            automation_dev_wait_text: browser.automation_dev_wait_text.clone(),
        }
    }
}
