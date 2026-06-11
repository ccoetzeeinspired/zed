//! In-editor embedded browser.
//!
//! Phase 1.A: a new browser tab can be opened via the `browser: new tab`
//! action; the tab hosts a WebView2 view inside Zed's window via
//! DirectComposition. See `plans/browser-viewer.md`.
//!
//! Gated to Windows; on other platforms `init` is a no-op so the workspace
//! cross-compiles cleanly.

use gpui::{App, actions};
use settings::Settings as _;
use ui::SharedString;
use workspace::Workspace;

pub mod browser_settings;
pub mod browser_view;
pub mod bundle;
pub mod design;
pub mod drawing;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub mod automation;
#[cfg(target_os = "windows")]
mod design_mode_script;
#[cfg(target_os = "windows")]
mod webview2_host;
#[cfg(target_os = "macos")]
mod wkwebview_host;

pub use browser_settings::BrowserSettings;
pub use browser_view::{BrowserItem, BrowserView, open_new_tab};

const NO_ACTIVE_BROWSER_MESSAGE: &str =
    "No active Zed browser tab. Open an embedded browser tab first.";

actions!(
    browser,
    [
        /// Open a new browser tab navigating to the configured homepage.
        NewTab,
        /// Open WebView2 DevTools on the active browser tab.
        OpenDevTools,
        /// Focus the address bar (Ctrl+L convention from real browsers).
        FocusAddressBar,
        /// Toggle design mode on the active browser tab.
        ToggleDesignMode,
        /// Toggle drawing mode — freehand strokes over the page.
        ToggleDrawingMode,
        /// Clear all drawn strokes on the active browser tab.
        ClearDrawing,
        /// CP0: run `Accessibility.enable` CDP smoke test on the active browser tab.
        AutomationSmokeTest,
        /// CP1: capture accessibility snapshot + ref registry on the active browser tab.
        AutomationSnapshot,
        /// CP3 dev: type email into ref e14 — uses `browser.automation_credentials.email`.
        AutomationTypeEmail,
        /// CP3 dev: type password into ref e18 — uses `browser.automation_credentials.password`.
        AutomationTypePassword,
        /// CP2 dev: click Sign In (ref e21 on TrueLens login page).
        AutomationClickSignIn,
        /// CP4 dev: navigate active browser tab to configured homepage.
        AutomationNavigateHomepage,
        /// CP4 dev: wait until navigation completes (no text check).
        AutomationWaitForLoad,
        /// CP4 dev: wait for load + text (`browser.automation_dev_wait_text`, default TrueLens).
        AutomationWaitForText,
        /// CP2: click a snapshot ref (`ZED_BROWSER_AUTOMATION_REF`, default e21). Prefer AutomationClickSignIn.
        AutomationClick,
        /// CP3: type into a snapshot ref (env overrides). Prefer AutomationTypeEmail / AutomationTypePassword.
        AutomationType
    ]
);

/// Register the browser-viewer feature with the application.
pub fn init(cx: &mut App) {
    BrowserSettings::register(cx);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    automation::init_automation_ipc(cx);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewTab, window, cx| {
            let homepage = BrowserSettings::get_global(cx).homepage.clone();
            open_new_tab(workspace, SharedString::new(homepage), window, cx);
        });
        #[cfg(target_os = "windows")]
        {
            workspace.register_action(|workspace, _: &AutomationSmokeTest, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                if let Err(err) = automation::run_smoke_test(browser, window, cx) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationSnapshot, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                if let Err(err) = automation::run_snapshot(browser, window, cx) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationTypeEmail, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let settings = BrowserSettings::get_global(cx);
                let Some(email) = settings.automation_email() else {
                    workspace.show_error(
                        &anyhow::anyhow!(
                            "Set browser.automation_credentials.email in your Zed settings (JSON) first."
                        ),
                        cx,
                    );
                    return;
                };
                log::info!("browser automation type email → ref {} ({email})", automation::DEV_EMAIL_REF);
                if let Err(err) = automation::run_type(
                    browser,
                    automation::DEV_EMAIL_REF,
                    &email,
                    window,
                    cx,
                ) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationTypePassword, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let settings = BrowserSettings::get_global(cx);
                let Some(password) = settings.automation_password() else {
                    workspace.show_error(
                        &anyhow::anyhow!(
                            "Set browser.automation_credentials.password in your Zed settings (JSON) first."
                        ),
                        cx,
                    );
                    return;
                };
                log::info!(
                    "browser automation type password → ref {} ({} chars)",
                    automation::DEV_PASSWORD_REF,
                    password.len()
                );
                if let Err(err) = automation::run_type(
                    browser,
                    automation::DEV_PASSWORD_REF,
                    &password,
                    window,
                    cx,
                ) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationClickSignIn, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                if let Err(err) = automation::run_click(
                    browser,
                    automation::DEV_SIGN_IN_REF,
                    window,
                    cx,
                ) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationNavigateHomepage, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let homepage = BrowserSettings::get_global(cx).homepage.clone();
                if let Err(err) =
                    automation::run_navigate_homepage(browser, &homepage, window, cx)
                {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationWaitForLoad, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                if let Err(err) = automation::run_wait_for(
                    browser,
                    automation::WaitForOptions {
                        wait_load: true,
                        text: None,
                        text_gone: None,
                        timeout: automation::DEFAULT_NAV_TIMEOUT,
                    },
                    window,
                    cx,
                ) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationWaitForText, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let text = BrowserSettings::get_global(cx)
                    .automation_dev_wait_text
                    .clone()
                    .unwrap_or_else(|| "TrueLens".to_string());
                if let Err(err) = automation::run_wait_for(
                    browser,
                    automation::WaitForOptions {
                        wait_load: true,
                        text: Some(text),
                        text_gone: None,
                        timeout: automation::DEFAULT_NAV_TIMEOUT,
                    },
                    window,
                    cx,
                ) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationClick, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let ref_id = automation::dev_automation_ref();
                if let Err(err) = automation::run_click(browser, &ref_id, window, cx) {
                    workspace.show_error(&err, cx);
                }
            });
            workspace.register_action(|workspace, _: &AutomationType, window, cx| {
                let Some(browser) = automation::resolve_automation_target(workspace, cx) else {
                    workspace.show_error(&anyhow::anyhow!(NO_ACTIVE_BROWSER_MESSAGE), cx);
                    return;
                };
                let settings = BrowserSettings::get_global(cx);
                let ref_id = automation::dev_automation_type_ref();
                let text = if ref_id == automation::DEV_EMAIL_REF {
                    settings.automation_email()
                } else if ref_id == automation::DEV_PASSWORD_REF {
                    settings.automation_password()
                } else {
                    settings
                        .automation_credential(&ref_id)
                        .or_else(|| {
                            let env_text = automation::dev_automation_text();
                            (!env_text.is_empty()).then_some(env_text)
                        })
                };
                let Some(text) = text else {
                    workspace.show_error(
                        &anyhow::anyhow!(
                            "No credential for ref {ref_id}. Set browser.automation_credentials in settings, or use browser: automation type email / type password."
                        ),
                        cx,
                    );
                    return;
                };
                if let Err(err) = automation::run_type(browser, &ref_id, &text, window, cx) {
                    workspace.show_error(&err, cx);
                }
            });
        }
    })
    .detach();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        log::info!("browser_viewer: skipping init (not supported on this platform)");
    }
}
