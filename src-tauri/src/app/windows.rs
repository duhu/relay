//! The settings and log windows.
//!
//! Both are the same Vue bundle behind a hash route, created on demand and
//! destroyed on close: the app keeps no UI state, the core holds all of it
//! (invariant 7 in `docs/overview.md`).

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Opens (or re-focuses) the window for `route`, which is also its label.
///
/// Relay is an `Accessory` app, so it has no Dock icon and macOS may leave a
/// freshly created window behind the frontmost app; the window is still there
/// and the next click brings it forward.
pub fn open(app: &AppHandle, route: &str) {
    if let Some(window) = app.get_webview_window(route) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    let (width, height) = match route {
        "log" => (640.0, 480.0),
        _ => (620.0, 700.0),
    };

    let url = WebviewUrl::App(format!("index.html#/{route}").into());
    match WebviewWindowBuilder::new(app, route, url)
        // The title set here only has to carry the moment between the window
        // appearing and the view loading, so it is the one word that reads the
        // same in both languages: each view sets its own localized title once
        // it knows the language, and again whenever the language changes.
        .title("Relay")
        .inner_size(width, height)
        .resizable(true)
        .build()
    {
        Ok(window) => {
            let _ = window.set_focus();
            tracing::info!(route, "window opened");
        }
        Err(err) => tracing::error!(route, error = %err, "cannot open the window"),
    }
}
