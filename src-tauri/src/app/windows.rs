//! The settings and log windows.
//!
//! Both are the same Vue bundle behind a hash route, created on demand and
//! hidden — not destroyed — on close: the app keeps no UI state that matters,
//! the core holds all of it (invariant 7 in `docs/overview.md`), but a window
//! that is only put away keeps the open tab, the scroll position and any edit
//! the user had typed and not yet saved.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

/// Sent to a window that was hidden and is now on screen again: what it shows
/// about the world outside it may have moved on while it was away.
const EVENT_SHOWN: &str = "shown";

/// Sent to a window as it is being put away, so it can stop any work — a poll
/// timer — that nobody is looking at.
const EVENT_HIDDEN: &str = "hidden";

/// Set once the user has asked the app to quit.
///
/// The close handler below hides a window instead of letting it close, which is
/// what the red button should do — but quitting asks every window to close on
/// its way out, and a hidden window that refused to close would keep the
/// process alive after the user asked it to quit. The tray's Quit sets this
/// first, so the handler steps aside and the windows really do close.
static QUITTING: AtomicBool = AtomicBool::new(false);

/// Tells the close handler to stop hiding windows: the app is on its way out.
///
/// Call this immediately before `AppHandle::exit`.
pub fn begin_quit() {
    QUITTING.store(true, Ordering::SeqCst);
}

/// Opens (or re-focuses) the window for `route`, which is also its label.
///
/// Relay is an `Accessory` app, so it has no Dock icon and macOS may leave a
/// freshly created window behind the frontmost app; the window is still there
/// and the next click brings it forward.
pub fn open(app: &AppHandle, route: &str) {
    if let Some(window) = app.get_webview_window(route) {
        let _ = window.show();
        let _ = window.set_focus();
        // The window may have been sitting hidden for days, holding whatever
        // the world looked like when it was put away; this is the view's cue to
        // re-read the parts of it that are not the user's own unsaved edit.
        let _ = window.emit_to(route, EVENT_SHOWN, ());
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
            hide_on_close(&window);
            let _ = window.set_focus();
            tracing::info!(route, "window opened");
        }
        Err(err) => tracing::error!(route, error = %err, "cannot open the window"),
    }
}

/// Makes the window's close button put it away instead of tearing it down.
///
/// Quitting is the tray's job (`tray.rs`), which is why this refuses to close
/// at all — except once [`begin_quit`] has been called, see [`QUITTING`].
fn hide_on_close(window: &WebviewWindow) {
    let handle = window.clone();
    window.on_window_event(move |event| {
        let WindowEvent::CloseRequested { api, .. } = event else {
            return;
        };
        if QUITTING.load(Ordering::SeqCst) {
            return;
        }
        api.prevent_close();
        let _ = handle.hide();
        let _ = handle.emit_to(handle.label(), EVENT_HIDDEN, ());
    });
}
