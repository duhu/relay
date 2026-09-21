//! The menu bar icon and its menu.
//!
//! The menu is rebuilt from scratch whenever the core publishes a new status,
//! because the host list itself changes when the config is reloaded.

use relay_core::config::LANG_EN;
use relay_core::runtime::{CoreHandle, Status};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Wry};

use super::windows;

/// Menu bar template icons: nothing happening, a switch under way, and
/// something the user has to deal with (no usable config, or no permission).
const TRAY_IDLE_ICON: &[u8] = include_bytes!("../../icons/tray-idle.png");
const TRAY_SWITCHING_ICON: &[u8] = include_bytes!("../../icons/tray-switching.png");
const TRAY_ATTENTION_ICON: &[u8] = include_bytes!("../../icons/tray-attention.png");

const MENU_ID_STATUS: &str = "status";
/// `switch:<host index>`; the index is the HID++ slot, not the channel number.
const MENU_ID_SWITCH_PREFIX: &str = "switch:";
const MENU_ID_SETTINGS: &str = "settings";
const MENU_ID_LOG: &str = "log";
const MENU_ID_QUIT: &str = "quit";

/// Builds the tray and keeps it in step with the core's status.
pub fn build(app: &AppHandle, core: CoreHandle) -> tauri::Result<()> {
    let mut updates = core.subscribe();
    let status = updates.borrow_and_update().clone();

    let for_menu = core.clone();
    let tray = TrayIconBuilder::<Wry>::new()
        .icon(icon_for(&status)?)
        .icon_as_template(true)
        .menu(&menu(app, &status)?)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| on_menu_event(app, &for_menu, event.id().as_ref()))
        .build(app)?;

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while updates.changed().await.is_ok() {
            let status = updates.borrow_and_update().clone();
            if let Err(err) = refresh(&app, &tray, &status) {
                tracing::warn!(error = %err, "the tray could not be updated");
            }
        }
    });

    Ok(())
}

fn refresh(app: &AppHandle, tray: &TrayIcon<Wry>, status: &Status) -> tauri::Result<()> {
    tray.set_menu(Some(menu(app, status)?))?;
    // `set_icon` followed by `set_icon_as_template` would flicker (each is a
    // separate redraw); this sets both atomically.
    tray.set_icon_with_as_template(Some(icon_for(status)?), true)
}

/// One line of the menu, before it becomes an AppKit object.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Separator,
    Item {
        id: String,
        label: String,
        enabled: bool,
    },
}

impl Row {
    fn item(id: impl Into<String>, label: impl Into<String>, enabled: bool) -> Self {
        Row::Item {
            id: id.into(),
            label: label.into(),
            enabled,
        }
    }
}

/// Picks the string the status asks for.
///
/// `Status::language` is always one of the concrete tags — relay-core has
/// already resolved `auto` — and the match is on English with Chinese as the
/// fallback, which is what `src/lib/i18n.ts` does: should a third language ever
/// arrive, the window and this menu fall back to the same one rather than to
/// two different ones.
fn pick(status: &Status, zh_hans: &'static str, en: &'static str) -> &'static str {
    if status.language == LANG_EN {
        en
    } else {
        zh_hans
    }
}

fn rows(status: &Status) -> Vec<Row> {
    let mut rows = vec![
        Row::item(
            MENU_ID_STATUS,
            format!(
                "{}{}",
                pick(status, "状态：", "Status: "),
                status_label(status)
            ),
            false,
        ),
        Row::Separator,
    ];

    // No hosts means no usable config, and nothing may offer a slot the config
    // does not declare (`AGENTS.md`).
    for (index, name) in &status.hosts {
        // The entry for the machine we are on is marked rather than removed:
        // clicking it is the one-click rescue that pulls the screen and the
        // follow devices back here, so it stays enabled.
        let label = if status.this_host == Some(*index) {
            if status.language == LANG_EN {
                format!("{name} (this Mac) ✓")
            } else {
                format!("{name}（本机）✓")
            }
        } else {
            let channel = index + 1;
            if status.language == LANG_EN {
                format!("Switch to {name} (channel {channel})")
            } else {
                format!("切到 {name}（通道 {channel}）")
            }
        };
        rows.push(Row::item(
            format!("{MENU_ID_SWITCH_PREFIX}{index}"),
            label,
            true,
        ));
    }
    if !status.hosts.is_empty() {
        rows.push(Row::Separator);
    }

    // The item does the same thing either way — it opens the one window — but
    // that window renders the wizard, not the settings form, on a Mac with
    // nothing to run on. Calling it "Settings…" there would send the user
    // somewhere the click does not go.
    rows.push(Row::item(MENU_ID_SETTINGS, settings_label(status), true));
    rows.push(Row::item(MENU_ID_LOG, pick(status, "日志…", "Log…"), true));
    rows.push(Row::item(MENU_ID_QUIT, pick(status, "退出", "Quit"), true));
    rows
}

fn menu(app: &AppHandle, status: &Status) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    for row in rows(status) {
        match row {
            Row::Separator => menu.append(&PredefinedMenuItem::separator(app)?)?,
            Row::Item { id, label, enabled } => {
                menu.append(&MenuItem::with_id(app, id, label, enabled, None::<&str>)?)?
            }
        }
    }
    Ok(menu)
}

fn on_menu_event(app: &AppHandle, core: &CoreHandle, id: &str) {
    match id {
        // The windows hide rather than close, so they have to be told the app
        // is leaving or they would refuse and keep the process alive; that is
        // `windows::quit`'s whole job, and why the exit goes through it.
        MENU_ID_QUIT => windows::quit(app),
        MENU_ID_SETTINGS => windows::open(app, "settings"),
        MENU_ID_LOG => windows::open(app, "log"),
        // The status line is disabled, so it never fires.
        MENU_ID_STATUS => {}
        _ => {
            let Some(target) = id
                .strip_prefix(MENU_ID_SWITCH_PREFIX)
                .and_then(|index| index.parse::<u8>().ok())
            else {
                tracing::warn!(id, "unknown tray menu item");
                return;
            };
            switch(core.clone(), target);
        }
    }
}

/// Runs the switch off the main thread; the result only goes to the log,
/// because a modal dialog over someone else's screen is worse than useless.
fn switch(core: CoreHandle, target: u8) {
    tauri::async_runtime::spawn(async move {
        match core.switch(target, "tray").await {
            Ok(report) if report.ok => tracing::info!(target, "switch done"),
            Ok(report) => {
                let failed: Vec<&str> = report
                    .steps
                    .iter()
                    .filter(|step| !step.ok)
                    .map(|step| step.what.as_str())
                    .collect();
                tracing::warn!(target, ?failed, "switch finished with failed steps");
            }
            Err(err) => tracing::error!(target, error = %err, "switch refused"),
        }
    });
}

/// What the status line says, in words that mean something to whoever reads
/// the menu rather than the name of the state the core is in.
///
/// A missing permission is otherwise only visible as a different icon, so the
/// line says it — but the config comes first: an unusable config is the thing
/// to fix, and its own line already points at the settings window.
fn status_label(status: &Status) -> &str {
    if status.config_ok && !status.input_monitoring {
        return pick(
            status,
            "缺少「输入监控」权限，点下面「设置」",
            "Input Monitoring is off — open Settings below",
        );
    }
    state_label(status)
}

/// What the window the item opens will actually show.
fn settings_label(status: &Status) -> &'static str {
    if status.state == "Unconfigured" {
        pick(status, "设置向导…", "Set Relay up…")
    } else {
        pick(status, "设置…", "Settings…")
    }
}

fn state_label(status: &Status) -> &str {
    match status.state.as_str() {
        "Idle" => pick(status, "就绪", "Ready"),
        "Confirming" => pick(
            status,
            "正在确认键盘是否真的走了…",
            "Checking whether the keyboard really left…",
        ),
        "Switching" => pick(status, "正在切换…", "Switching…"),
        "Cooldown" => pick(
            status,
            "刚切换过，几秒内不响应",
            "Just switched; ignoring triggers for a few seconds",
        ),
        // Says the name of the item below it, which on this state is the wizard.
        "Unconfigured" => pick(
            status,
            "未配置，点下面「设置向导…」",
            "Not configured — start the setup below",
        ),
        // An unknown state is the core's own word for it, in either language.
        other => other,
    }
}

fn icon_for(status: &Status) -> tauri::Result<Image<'static>> {
    Image::from_bytes(icon_bytes(status))
}

fn icon_bytes(status: &Status) -> &'static [u8] {
    match status.state.as_str() {
        "Confirming" | "Switching" => TRAY_SWITCHING_ICON,
        // Something needs the user: no usable config, or no permission to read
        // the devices a later milestone switches natively.
        _ if !status.config_ok || !status.input_monitoring => TRAY_ATTENTION_ICON,
        _ => TRAY_IDLE_ICON,
    }
}

#[cfg(test)]
mod tests {
    use relay_core::config::LANG_ZH_HANS;

    use super::*;

    fn status(state: &str, hosts: Vec<(u8, String)>) -> Status {
        Status {
            state: state.to_string(),
            config_ok: !hosts.is_empty(),
            config_error: None,
            this_host: hosts.first().map(|(index, _)| *index),
            hosts,
            input_monitoring: true,
            last_report: None,
            language: LANG_ZH_HANS,
        }
    }

    /// The same status, in the other language.
    fn in_english(mut status: Status) -> Status {
        status.language = LANG_EN;
        status
    }

    fn two_hosts() -> Vec<(u8, String)> {
        vec![(1, "Bam.Work".to_string()), (2, "Bam.Mini".to_string())]
    }

    #[test]
    fn a_configured_menu_offers_every_declared_host() {
        assert_eq!(
            rows(&status("Idle", two_hosts())),
            vec![
                Row::item(MENU_ID_STATUS, "状态：就绪", false),
                Row::Separator,
                // The slot is 0-based; only the channel the user sees is +1.
                Row::item("switch:1", "Bam.Work（本机）✓", true),
                Row::item("switch:2", "切到 Bam.Mini（通道 3）", true),
                Row::Separator,
                Row::item(MENU_ID_SETTINGS, "设置…", true),
                Row::item(MENU_ID_LOG, "日志…", true),
                Row::item(MENU_ID_QUIT, "退出", true),
            ]
        );
    }

    #[test]
    fn an_english_menu_says_the_same_thing_in_english() {
        assert_eq!(
            rows(&in_english(status("Idle", two_hosts()))),
            vec![
                Row::item(MENU_ID_STATUS, "Status: Ready", false),
                Row::Separator,
                Row::item("switch:1", "Bam.Work (this Mac) ✓", true),
                Row::item("switch:2", "Switch to Bam.Mini (channel 3)", true),
                Row::Separator,
                Row::item(MENU_ID_SETTINGS, "Settings…", true),
                Row::item(MENU_ID_LOG, "Log…", true),
                Row::item(MENU_ID_QUIT, "Quit", true),
            ]
        );
    }

    /// A tag neither this menu nor `src/lib/i18n.ts` has strings for must land
    /// on the same fallback in both, so the window and the menu cannot end up
    /// speaking different languages.
    #[test]
    fn an_unknown_language_falls_back_to_chinese() {
        let mut unknown = status("Idle", two_hosts());
        unknown.language = "de-DE";

        assert_eq!(rows(&unknown), rows(&status("Idle", two_hosts())));
    }

    #[test]
    fn the_machine_we_are_on_is_marked_and_still_clickable() {
        let mut here = status("Idle", two_hosts());
        here.this_host = Some(2);

        assert_eq!(
            rows(&here)[2..4],
            [
                // Every other host keeps the plain "switch to" wording.
                Row::item("switch:1", "切到 Bam.Work（通道 2）", true),
                // Clicking this one is the rescue that pulls the screen back,
                // so it must stay enabled.
                Row::item("switch:2", "Bam.Mini（本机）✓", true),
            ]
        );
    }

    #[test]
    fn a_menu_with_no_local_host_marks_nothing() {
        let mut elsewhere = status("Idle", two_hosts());
        elsewhere.this_host = None;

        assert_eq!(
            rows(&elsewhere)[2..4],
            [
                Row::item("switch:1", "切到 Bam.Work（通道 2）", true),
                Row::item("switch:2", "切到 Bam.Mini（通道 3）", true),
            ]
        );
    }

    #[test]
    fn an_english_menu_marks_the_machine_we_are_on_too() {
        let mut here = in_english(status("Idle", two_hosts()));
        here.this_host = Some(2);

        assert_eq!(
            rows(&here)[2..4],
            [
                Row::item("switch:1", "Switch to Bam.Work (channel 2)", true),
                Row::item("switch:2", "Bam.Mini (this Mac) ✓", true),
            ]
        );
    }

    #[test]
    fn an_unconfigured_menu_offers_no_host_at_all() {
        assert_eq!(
            rows(&status("Unconfigured", Vec::new())),
            vec![
                Row::item(MENU_ID_STATUS, "状态：未配置，点下面「设置向导…」", false),
                Row::Separator,
                // The item names the wizard, which is what this window shows
                // on a Mac that has nothing to run on.
                Row::item(MENU_ID_SETTINGS, "设置向导…", true),
                Row::item(MENU_ID_LOG, "日志…", true),
                Row::item(MENU_ID_QUIT, "退出", true),
            ]
        );
    }

    #[test]
    fn an_unconfigured_english_menu_offers_no_host_at_all() {
        assert_eq!(
            rows(&in_english(status("Unconfigured", Vec::new()))),
            vec![
                Row::item(
                    MENU_ID_STATUS,
                    "Status: Not configured — start the setup below",
                    false
                ),
                Row::Separator,
                Row::item(MENU_ID_SETTINGS, "Set Relay up…", true),
                Row::item(MENU_ID_LOG, "Log…", true),
                Row::item(MENU_ID_QUIT, "Quit", true),
            ]
        );
    }

    #[test]
    fn the_status_line_says_what_the_core_is_doing() {
        for (state, line) in [
            ("Idle", "状态：就绪"),
            ("Confirming", "状态：正在确认键盘是否真的走了…"),
            ("Switching", "状态：正在切换…"),
            ("Cooldown", "状态：刚切换过，几秒内不响应"),
            // An unknown state still shows something rather than nothing.
            ("Whatever", "状态：Whatever"),
        ] {
            assert_eq!(
                rows(&status(state, two_hosts()))[0],
                Row::item(MENU_ID_STATUS, line, false)
            );
        }
    }

    #[test]
    fn the_english_status_line_says_what_the_core_is_doing() {
        for (state, line) in [
            ("Idle", "Status: Ready"),
            (
                "Confirming",
                "Status: Checking whether the keyboard really left…",
            ),
            ("Switching", "Status: Switching…"),
            (
                "Cooldown",
                "Status: Just switched; ignoring triggers for a few seconds",
            ),
            // An unknown state still shows something rather than nothing.
            ("Whatever", "Status: Whatever"),
        ] {
            assert_eq!(
                rows(&in_english(status(state, two_hosts())))[0],
                Row::item(MENU_ID_STATUS, line, false)
            );
        }
    }

    #[test]
    fn a_missing_permission_shows_up_in_the_status_line() {
        let mut unpermitted = status("Idle", two_hosts());
        unpermitted.input_monitoring = false;

        assert_eq!(
            rows(&unpermitted)[0],
            Row::item(
                MENU_ID_STATUS,
                "状态：缺少「输入监控」权限，点下面「设置」",
                false
            )
        );
    }

    #[test]
    fn a_missing_permission_shows_up_in_the_english_status_line() {
        let mut unpermitted = in_english(status("Idle", two_hosts()));
        unpermitted.input_monitoring = false;

        assert_eq!(
            rows(&unpermitted)[0],
            Row::item(
                MENU_ID_STATUS,
                "Status: Input Monitoring is off — open Settings below",
                false
            )
        );
    }

    #[test]
    fn an_unusable_config_beats_a_missing_permission() {
        let mut neither = status("Unconfigured", Vec::new());
        neither.input_monitoring = false;

        assert_eq!(
            rows(&neither)[0],
            Row::item(MENU_ID_STATUS, "状态：未配置，点下面「设置向导…」", false)
        );
    }

    #[test]
    fn the_icon_follows_the_state() {
        assert_eq!(icon_bytes(&status("Idle", two_hosts())), TRAY_IDLE_ICON);
        assert_eq!(icon_bytes(&status("Cooldown", two_hosts())), TRAY_IDLE_ICON);
        assert_eq!(
            icon_bytes(&status("Confirming", two_hosts())),
            TRAY_SWITCHING_ICON
        );
        assert_eq!(
            icon_bytes(&status("Switching", two_hosts())),
            TRAY_SWITCHING_ICON
        );
        assert_eq!(
            icon_bytes(&status("Unconfigured", Vec::new())),
            TRAY_ATTENTION_ICON
        );

        let mut unpermitted = status("Idle", two_hosts());
        unpermitted.input_monitoring = false;
        assert_eq!(icon_bytes(&unpermitted), TRAY_ATTENTION_ICON);
    }
}
