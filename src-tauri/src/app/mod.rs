//! The resident app: single instance, core runtime, IPC server and tray.

mod commands;
pub mod paths;
mod tray;
mod windows;

use std::fs::{self, File};
use std::io;
use std::sync::Arc;

use relay_core::config::Config;
use relay_core::permissions;
use relay_core::plan::SwitchPlan;
use relay_core::runtime::{Core, CoreHandle};
use tauri::{App, AppHandle, Manager};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use crate::ipc::{self, Request, Response};

/// How many log entries the settings window's Log view can scroll back through.
const LOG_CAPACITY: usize = 200;

/// Run the menu bar app. Blocks until the app exits.
pub fn run() {
    // Released when the process ends, so it has to outlive `Builder::run`.
    let _lock = match acquire_single_instance_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("Relay is already running");
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("Relay: cannot take {}: {err}", paths::lock_path().display());
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        // The settings window's export uses the native save panel to pick a
        // path; only the panel lives in the frontend, the copy stays in Rust.
        .plugin(tauri_plugin_dialog::init())
        // Registered here rather than on each window, so that every window is
        // covered from birth: `windows::open` also runs off the main thread.
        .on_window_event(windows::hide_on_close)
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::get_status,
            commands::trigger_switch,
            commands::get_logs,
            commands::list_hid_devices,
            commands::scan_devices,
            commands::list_displays,
            commands::read_display_input,
            commands::list_input_sources,
            commands::input_monitoring_granted,
            commands::request_input_monitoring,
            commands::open_privacy_settings,
            commands::read_config_file,
            commands::import_config,
            commands::export_config,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            configure_activation_policy(app);

            let core = start_core();
            // The settings window's commands reach the core through this.
            app.manage(core.clone());
            serve_ipc(app.handle(), core.clone());
            tray::build(app.handle(), core.clone())?;
            configure_autostart(app.handle(), core.clone());
            prompt_for_input_monitoring(core.clone());
            open_settings_if_unconfigured(app.handle(), core);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start the Relay menu bar app");
}

/// Takes the lock that makes this the only resident app (spec §7).
///
/// `Ok(None)` means another instance holds it. The lock lives as long as the
/// returned file is open, so the caller has to keep it.
fn acquire_single_instance_lock() -> io::Result<Option<File>> {
    let path = paths::lock_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(err) if err.kind() == fs2::lock_contended_error().kind() => Ok(None),
        Err(err) => Err(err),
    }
}

/// Starts logging and the core loop.
///
/// A Mac that has never been configured has no config file, and nothing here
/// invents one: the core comes up `Unconfigured` and
/// [`open_settings_if_unconfigured`] sends the user to the wizard, which writes
/// the first file there will ever be.
fn start_core() -> CoreHandle {
    let log = relay_core::log::init(&paths::log_dir(), LOG_CAPACITY);
    let config_path = paths::config_path();

    // `Core::start` spawns its loop onto the ambient tokio runtime, so it has
    // to run inside Tauri's rather than on the bare setup thread.
    tauri::async_runtime::block_on(async move { Core::start(config_path, log) })
}

/// Puts the settings window on screen when there is nothing for the core to
/// run on, so a fresh Mac opens the wizard by itself instead of leaving a tray
/// icon that answers every click with "not configured".
///
/// The three ways to have nothing to run on are one question to the core: no
/// config file at all, a file that does not load or validate, and a config
/// whose `this_host` names a slot no host declares all leave `config_ok` false
/// — the last of them is checked on its own as well, because a `this_host`
/// nobody declares is exactly the state an interrupted import leaves behind.
///
/// The frontend decides on its own whether that window renders the wizard or
/// the settings form, so there is no route to ask for here.
///
/// Asking waits for the core's loop to answer, which it cannot do until
/// Tauri's runtime is running, so — like [`prompt_for_input_monitoring`] —
/// this spawns and lets `setup` return.
fn open_settings_if_unconfigured(app: &AppHandle, core: CoreHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let status = core.status().await;
        let declared = status
            .this_host
            .is_some_and(|this| status.hosts.iter().any(|(index, _)| *index == this));
        if status.config_ok && declared {
            return;
        }
        tracing::info!(
            config_ok = status.config_ok,
            this_host_declared = declared,
            "nothing to run on yet; opening the settings window for the wizard",
        );
        windows::open(&app, "settings");
    });
}

/// Serves the CLI's unix socket for as long as the app runs.
fn serve_ipc(app: &AppHandle, core: CoreHandle) {
    let socket = paths::sock_path();
    let handler = handler(app.clone(), core);
    tauri::async_runtime::spawn(async move {
        if let Err(err) = ipc::serve(&socket, handler).await {
            tracing::error!(
                socket = %socket.display(),
                error = %err,
                "the IPC server stopped; the CLI cannot reach this app",
            );
        }
    });
}

fn handler(app: AppHandle, core: CoreHandle) -> ipc::Handler {
    Arc::new(move |request| {
        let app = app.clone();
        let core = core.clone();
        Box::pin(async move { answer(app, core, request).await })
    })
}

async fn answer(app: AppHandle, core: CoreHandle, request: Request) -> Response {
    match request {
        Request::Status => match serde_json::to_value(core.status().await) {
            Ok(status) => Response::ok(status),
            Err(err) => Response::error(err),
        },
        Request::Switch {
            target,
            dry_run: true,
        } => match core.dry_run(target).await {
            Ok(plan) => Response::ok(plan_json(&plan)),
            Err(err) => Response::error(err),
        },
        Request::Switch {
            target,
            dry_run: false,
        } => match core.switch(target, "cli").await {
            Ok(report) => match serde_json::to_value(report) {
                Ok(report) => Response::ok(report),
                Err(err) => Response::error(err),
            },
            Err(err) => Response::error(err),
        },
        Request::OpenSettings => {
            windows::open(&app, "settings");
            Response::ok(serde_json::Value::Null)
        }
        Request::ReloadConfig => match core.reload().await {
            Ok(()) => Response::ok(serde_json::Value::Null),
            Err(err) => Response::error(err),
        },
    }
}

/// `SwitchPlan` is a plain core type with no `Serialize`, so the wire shape
/// lives here, next to the CLI that reads it back.
pub(crate) fn plan_json(plan: &SwitchPlan) -> serde_json::Value {
    serde_json::json!({
        "target": plan.target,
        "displays": plan
            .displays
            .iter()
            .map(|(name, input)| serde_json::json!({ "name": name, "input": input }))
            .collect::<Vec<_>>(),
        "devices": plan.devices,
    })
}

/// Keeps the login item in step with `options.launch_at_login`: now, and
/// again whenever a config reload makes the core publish a new status.
///
/// Only a change is handed to the plugin — a status tick that leaves the
/// option alone must not rewrite the launch agent.
fn configure_autostart(app: &AppHandle, core: CoreHandle) {
    let wanted = launch_at_login();
    let mut applied = apply_autostart(app, wanted).then_some(wanted);

    let app = app.clone();
    let mut updates = core.subscribe();
    tauri::async_runtime::spawn(async move {
        while updates.changed().await.is_ok() {
            let wanted = launch_at_login();
            if applied == Some(wanted) {
                continue;
            }
            applied = apply_autostart(&app, wanted).then_some(wanted);
        }
    });
}

/// `options.launch_at_login` as the file has it. A config that does not load
/// yet — a Mac still in the wizard — gets the serde default, which is on.
fn launch_at_login() -> bool {
    Config::load(&paths::config_path())
        .map(|config| config.options.launch_at_login)
        .unwrap_or(true)
}

/// Turns the login item on or off; `false` means the plugin refused, so the
/// caller keeps looking for the change it could not make.
fn apply_autostart(app: &AppHandle, enabled: bool) -> bool {
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    match result {
        Ok(()) => {
            tracing::info!(enabled, "login item updated");
            true
        }
        Err(err) => {
            tracing::warn!(enabled, error = %err, "cannot update the login item");
            false
        }
    }
}

/// Shows the Input Monitoring prompt once on a machine that has not answered
/// it. The call blocks until the user does, so it cannot run on setup's thread.
///
/// When the user grants it, the core is told to reload so it republishes
/// `Status` with `input_monitoring: true` — otherwise the tray icon would sit
/// in its attention state until something else happened to trigger a reload.
///
/// A Mac with no config file is the one machine this skips. There the wizard is
/// about to open, and its first step is this same permission with a screen
/// saying what it is for; asking here would put the system dialog on screen
/// first, with nothing behind it to explain itself, and leave that step with
/// nothing to say but "granted".
fn prompt_for_input_monitoring(core: CoreHandle) {
    if permissions::input_monitoring_granted() || !paths::config_path().exists() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let granted = tauri::async_runtime::spawn_blocking(permissions::request_input_monitoring)
            .await
            .unwrap_or(false);
        tracing::info!(granted, "the Input Monitoring prompt was answered");

        if granted {
            match core.reload().await {
                Ok(()) => {
                    tracing::info!("the core was reloaded after Input Monitoring was granted")
                }
                Err(err) => tracing::warn!(
                    error = %err,
                    "cannot reload the core after Input Monitoring was granted"
                ),
            }
        }
    });
}

/// Keep Relay out of the Dock and the app switcher.
///
/// `Info.plist` already sets `LSUIElement`, but tao activates the process as
/// `.regular` with `activateIgnoringOtherApps(true)` before this hook runs. For
/// a window-less status bar app that makes macOS implicitly close the tray menu
/// on the first submenu hover, so undo the activation here as well.
#[cfg(target_os = "macos")]
fn configure_activation_policy(app: &mut App) {
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let mtm = objc2_foundation::MainThreadMarker::new()
        .expect("Tauri's setup hook runs on the main thread");
    let ns_app = objc2_app_kit::NSApp(mtm);
    ns_app.deactivate();
    #[allow(deprecated)]
    ns_app.activateIgnoringOtherApps(false);
}
