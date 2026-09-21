// The window's two dictionaries and the lookup the views call.
//
// No third-party library: the app has two windows and one flat list of keys,
// and the language always comes from `Status.language`, which relay-core has
// already resolved from `options.language` (so `"auto"` never reaches here and
// the tray menu cannot disagree with the window).
//
// A missing key returns the key and warns rather than falling back to Chinese:
// a hole in the English dictionary has to be visible, not invisible.

import { ref } from "vue";

/** The concrete tags `Status.language` can carry. */
export const LANGS = ["zh-Hans", "en"] as const;

export type Lang = (typeof LANGS)[number];

/**
 * What an unknown tag falls back to. The same choice relay-core makes when it
 * cannot read the system preference.
 */
const FALLBACK: Lang = "zh-Hans";

const zhHans: Record<string, string> = {
  // Shared words.
  "common.name": "名称",
  "common.unnamed": "未命名",
  "common.unknown": "未知",
  "common.delete": "删除",
  "common.add": "添加",
  "common.none": "无",
  "common.save": "保存",
  "common.ms": "毫秒",

  // The window header and its four tabs.
  "app.title": "Relay 设置",
  "tab.overview": "概览",
  "tab.machines": "机器与屏幕",
  "tab.devices": "键盘鼠标",
  "tab.advanced": "高级",

  // `Status.state`, short enough for the header line. The tray says the same
  // things at greater length; here there is room for two words.
  "state.idle": "就绪",
  "state.confirming": "确认中",
  "state.switching": "切换中",
  "state.cooldown": "冷却中",
  "state.unconfigured": "未配置",

  // The overview tab.
  "overview.thisMac": "本机",
  "overview.channelInput": "通道 {channel} · 输入源 {input}",
  "overview.channelOnly": "通道 {channel}",
  "overview.screen": "画面",
  "overview.here": "在本机",
  "overview.screenHere": "画面在本机",
  "overview.screenOn": "画面在 {name}",
  "overview.screenUnknown": "画面位置未知",
  "overview.lastSeen": "上次切到 {name}",
  "overview.manual": "手动切换",
  "overview.switchTo": "切过去",
  "overview.pullBack": "取回画面和鼠标",
  "overview.switching": "切换中…",
  "overview.lastSwitch": "最近一次切换 · 去 {host}",
  "overview.someStepsFailed": "有步骤失败",
  "overview.noHosts": "还没有机器，去「机器与屏幕」添加。",
  "overview.configInactive": "配置未生效",
  "overview.configInactiveDetail": "配置未生效：{detail}",

  // The steps of the last switch. relay-core writes them in English — the log
  // and the CLI read the very same strings — so the window translates the
  // shapes `executor.rs` produces and shows anything else untouched.
  "step.display": "显示器 {name}",
  "step.device": "设备 {name}",
  "step.input": "输入源 {input}",
  "step.switched": "已切到 {name}",
  "step.alreadyAtTarget": "已经在目标机",
  "step.skipped": "不在本机，已跳过",
  "step.deviceNotPresent": "设备不在本机",
  "step.displayNotFound": "找不到这台显示器",
  "step.deviceNotFound": "找不到这台设备",
  "step.displayTimeout": "显示器没有及时回应",
  "step.deviceTimeout": "设备没有及时回应",
  "step.beyond": "通道 {channel} 超出设备的 {count} 个插槽",

  // The input monitoring row.
  "permission.title": "权限",
  "permission.label": "输入监控",
  "permission.granted": "已授权",
  "permission.denied": "未授权",
  "permission.request": "请求授权",
  "permission.openSettings": "打开系统设置",

  // The hosts table.
  "hosts.title": "机器",
  "hosts.channel": "通道",
  "hosts.thisMac": "本机",
  "hosts.channelHint": "通道号就是键鼠上的 Easy-Switch 按键。",
  "hosts.add": "＋ 添加机器",
  "hosts.newName": "机器 {channel}",
  "hosts.label": "{name}（通道 {channel}）",

  // The displays table.
  "displays.title": "显示器",
  "displays.nameColumn": "名称 / EDID",
  "displays.edid": "EDID",
  "displays.edidHint": "留空表示第一块外接显示器",
  "displays.inputHint": "这台机器占用屏幕时要切到的 DDC 输入源编号",
  "displays.readTitle": "读取这台显示器当前的输入源",
  "displays.read": "读取",
  "displays.reading": "…",
  "displays.add": "＋ 添加显示器",
  "displays.scan": "扫描显示器",
  "displays.noneFound": "没有扫描到外接显示器。",
  "displays.readUnsupported": "这台显示器不回答输入源，请手动填写编号。",
  "displays.readNoMatch": "本机没有 EDID UUID 是「{uuid}」的显示器，请用「扫描显示器」核对这一行。",
  "displays.rowGone": "这一行已经不在了，读到的输入源没有写进去。",

  // The devices table.
  "devices.title": "设备",
  "devices.transport": "连接",
  "devices.trigger": "触发",
  "devices.triggerTitle": "它离开本机时触发切换（一台设备只能选一个）",
  "devices.follow": "跟随",
  "devices.followTitle": "切换时被送去目标机（一台设备只能选一个）",
  "devices.leaveTo": "离开去",
  "devices.leaveToTitle": "三台机器以上时，触发设备必须指定离开后去哪台",
  "devices.idTitle": "设备 ID（vid:pid）",
  "devices.serial": "序列号 {serial}",
  "devices.add": "＋ 添加设备",
  "devices.scan": "扫描设备",
  "devices.scanNeedsPermission": "扫描可切换设备需要「输入监控」授权，请先在上面授权。",
  "devices.noneFound": "没有扫描到可切换的设备。",
  "devices.noSerial": "无序列号",
  "devices.currentHost": "当前主机 通道 {channel} / 共 {count}",
  "devices.roleKeyboard": "键盘",
  "devices.roleMouse": "鼠标",
  "devices.roleOther": "其它",

  // The two scan buttons share these.
  "scan.collapse": "收起扫描结果",
  "scan.scanning": "扫描中…",

  // The timing card.
  "timing.title": "时间",
  "timing.debounce": "防抖",
  "timing.debounceHint": "键盘离开后等多久才动",
  "timing.cooldown": "冷却",
  "timing.cooldownHint": "切换后多久内不响应",
  "timing.retries": "显示器重试次数",

  // The behaviour and language cards.
  "options.title": "行为",
  "options.launchAtLogin": "登录时自动启动",
  "options.switchBack": "键盘意外连回本机时自动切回",
  "options.pullOnArrival": "键盘连到本机时自动取回画面",
  "options.languageTitle": "语言",
  "options.language": "界面语言",
  "options.languageAuto": "跟随系统",
  // The two language names are endonyms: the same in either dictionary, so
  // whoever cannot read the current one can still find their own.
  "options.languageZhHans": "简体中文",
  "options.languageEn": "English",

  // The footer.
  "footer.savedNote": "保存后立即生效",

  // What the shape check says before the backend ever sees the config.
  "validate.hostChannel": "第 {row} 行机器的通道号必须是 1–3 的整数。",
  "validate.displayInput": "显示器「{name}」在通道 {channel} 的输入源必须是 0–255 的整数。",
  "validate.debounce": "防抖时间至少 {ms} ms，且必须是整数。",
  "validate.cooldown": "冷却时间至少 {ms} ms，且必须是整数。",
  "validate.retries": "显示器重试次数必须是 0–255 的整数。",

  // Toasts.
  "toast.saved": "已保存",
  "toast.channelRange": "通道号必须在 1–3 之间",
  "toast.channelTaken": "通道 {channel} 已被「{name}」使用，请先改另一台",
  "toast.readInput": "读到当前输入源 {code}",

  // Banners. The detail is whatever the backend said, which is still English
  // (the Rust side's `ConfigError` is not translated yet).
  "error.loadConfig": "无法读取配置文件：{detail}",
  "error.save": "保存失败：{detail}",
  "error.loadPermission": "无法读取输入监控状态：{detail}",
  "error.requestPermission": "无法请求输入监控授权：{detail}",
  "error.openSettings": "无法打开系统设置：{detail}",
  "error.scanDisplays": "扫描显示器失败：{detail}",
  "error.readInput": "读取输入源失败：{detail}",
  "error.scanDevices": "扫描设备失败：{detail}",
  "error.switch": "切换失败：{detail}",

  // The log window.
  "log.title": "Relay 日志",
  "log.empty": "还没有日志。",
  "log.loadFailed": "无法读取日志：{detail}",
};

const en: Record<string, string> = {
  "common.name": "Name",
  "common.unnamed": "Unnamed",
  "common.unknown": "Unknown",
  "common.delete": "Delete",
  "common.add": "Add",
  "common.none": "None",
  "common.save": "Save",
  "common.ms": "ms",

  "app.title": "Relay Settings",
  "tab.overview": "Overview",
  "tab.machines": "Macs & displays",
  "tab.devices": "Keyboard & mouse",
  "tab.advanced": "Advanced",

  "state.idle": "Ready",
  "state.confirming": "Checking",
  "state.switching": "Switching",
  "state.cooldown": "Cooling down",
  "state.unconfigured": "Not configured",

  "overview.thisMac": "This Mac",
  "overview.channelInput": "Channel {channel} · input source {input}",
  "overview.channelOnly": "Channel {channel}",
  "overview.screen": "Screen",
  "overview.here": "On this Mac",
  "overview.screenHere": "Screen on this Mac",
  "overview.screenOn": "Screen on {name}",
  "overview.screenUnknown": "Screen location unknown",
  "overview.lastSeen": "Last switched to {name}",
  "overview.manual": "Switch manually",
  "overview.switchTo": "Switch there",
  "overview.pullBack": "Pull back here",
  "overview.switching": "Switching…",
  "overview.lastSwitch": "Last switch · to {host}",
  "overview.someStepsFailed": "Some steps failed",
  "overview.noHosts": "No Macs yet; add one under “Macs & displays”.",
  "overview.configInactive": "Config not in effect",
  "overview.configInactiveDetail": "Config not in effect: {detail}",

  "step.display": "Display {name}",
  "step.device": "Device {name}",
  "step.input": "Input source {input}",
  "step.switched": "Switched to {name}",
  "step.alreadyAtTarget": "Already on the target",
  "step.skipped": "Not on this Mac, skipped",
  "step.deviceNotPresent": "Device not present",
  "step.displayNotFound": "Display not found",
  "step.deviceNotFound": "Device not found",
  "step.displayTimeout": "Display did not answer in time",
  "step.deviceTimeout": "Device did not answer in time",
  "step.beyond": "Channel {channel} is beyond the device's {count} slots",

  "permission.title": "Permission",
  "permission.label": "Input Monitoring",
  "permission.granted": "Granted",
  "permission.denied": "Not granted",
  "permission.request": "Request access",
  "permission.openSettings": "Open System Settings",

  "hosts.title": "Macs",
  "hosts.channel": "Channel",
  "hosts.thisMac": "This Mac",
  "hosts.channelHint": "The channel is the Easy-Switch key on the keyboard and the mouse.",
  "hosts.add": "＋ Add a Mac",
  "hosts.newName": "Mac {channel}",
  "hosts.label": "{name} (channel {channel})",

  "displays.title": "Displays",
  "displays.nameColumn": "Name / EDID",
  "displays.edid": "EDID",
  "displays.edidHint": "Empty means the first external display",
  "displays.inputHint": "The DDC input source to switch to when that Mac owns the screen",
  "displays.readTitle": "Read this display's current input source",
  "displays.read": "Read",
  "displays.reading": "…",
  "displays.add": "＋ Add a display",
  "displays.scan": "Scan displays",
  "displays.noneFound": "No external display found.",
  "displays.readUnsupported": "This display will not report its input source; type the number in.",
  "displays.readNoMatch":
    "No display on this Mac has the EDID UUID “{uuid}”; check this row with “Scan displays”.",
  "displays.rowGone": "That row is gone, so the input source that came back was not written.",

  "devices.title": "Devices",
  "devices.transport": "Link",
  "devices.trigger": "Trigger",
  "devices.triggerTitle": "Its leaving this Mac starts a switch (pick one per device)",
  "devices.follow": "Follow",
  "devices.followTitle": "It is handed to the target Mac on every switch (pick one per device)",
  "devices.leaveTo": "Leaves to",
  "devices.leaveToTitle": "With three Macs or more, every trigger device needs one",
  "devices.idTitle": "Device id (vid:pid)",
  "devices.serial": "Serial {serial}",
  "devices.add": "＋ Add a device",
  "devices.scan": "Scan devices",
  "devices.scanNeedsPermission":
    "Scanning for switchable devices needs Input Monitoring; grant it above first.",
  "devices.noneFound": "No switchable device found.",
  "devices.noSerial": "No serial",
  "devices.currentHost": "Now on channel {channel} of {count}",
  "devices.roleKeyboard": "Keyboard",
  "devices.roleMouse": "Mouse",
  "devices.roleOther": "Other",

  "scan.collapse": "Hide scan results",
  "scan.scanning": "Scanning…",

  "timing.title": "Timing",
  "timing.debounce": "Debounce",
  "timing.debounceHint": "How long to wait after the keyboard leaves",
  "timing.cooldown": "Cooldown",
  "timing.cooldownHint": "How long to ignore triggers after a switch",
  "timing.retries": "Display retries",

  "options.title": "Behavior",
  "options.launchAtLogin": "Launch at login",
  "options.switchBack": "Switch back when the keyboard reconnects here by accident",
  "options.pullOnArrival": "Pull the screen back when the keyboard arrives here",
  "options.languageTitle": "Language",
  "options.language": "Interface language",
  "options.languageAuto": "Follow the system",
  "options.languageZhHans": "简体中文",
  "options.languageEn": "English",

  "footer.savedNote": "Changes take effect immediately",

  "validate.hostChannel": "The channel of Mac {row} has to be a whole number from 1 to 3.",
  "validate.displayInput":
    "The input source of display “{name}” on channel {channel} has to be a whole number from 0 to 255.",
  "validate.debounce": "The debounce has to be a whole number of at least {ms} ms.",
  "validate.cooldown": "The cooldown has to be a whole number of at least {ms} ms.",
  "validate.retries": "Display retries has to be a whole number from 0 to 255.",

  "toast.saved": "Saved",
  "toast.channelRange": "The channel has to be 1, 2 or 3",
  "toast.channelTaken": "Channel {channel} already belongs to “{name}”; change that one first",
  "toast.readInput": "Read input source {code}",

  "error.loadConfig": "Cannot read the config file: {detail}",
  "error.save": "Saving failed: {detail}",
  "error.loadPermission": "Cannot read the Input Monitoring state: {detail}",
  "error.requestPermission": "Cannot ask for Input Monitoring: {detail}",
  "error.openSettings": "Cannot open System Settings: {detail}",
  "error.scanDisplays": "Scanning displays failed: {detail}",
  "error.readInput": "Reading the input source failed: {detail}",
  "error.scanDevices": "Scanning devices failed: {detail}",
  "error.switch": "The switch failed: {detail}",

  "log.title": "Relay Log",
  "log.empty": "No log lines yet.",
  "log.loadFailed": "Cannot read the log: {detail}",
};

const DICTIONARIES: Record<Lang, Record<string, string>> = { "zh-Hans": zhHans, en };

/** A ref, so every `t()` in a template re-renders when the language changes. */
const current = ref<Lang>(FALLBACK);

function isLang(lang: string): lang is Lang {
  return (LANGS as readonly string[]).includes(lang);
}

/** The language in use; `Status.language` is what the views pass in. */
export function language(): Lang {
  return current.value;
}

/**
 * Switches the whole window to `lang`. A tag this build has no dictionary for
 * is a bug on the Rust side, so it warns and falls back rather than crashing.
 *
 * `<html lang>` follows along: it is what the webview picks the CJK vs Latin
 * font from, and what a screen reader reads the page in. `index.html` ships a
 * Chinese tag for the first paint, which is the same language `FALLBACK` is.
 */
export function setLanguage(lang: string): void {
  if (isLang(lang)) {
    current.value = lang;
  } else {
    console.warn(`[i18n] unknown language ${lang}; falling back to ${FALLBACK}`);
    current.value = FALLBACK;
  }
  // Both tags are valid BCP-47 as they stand, so no mapping is needed.
  document.documentElement.lang = current.value;
}

/**
 * One string, with `{name}` placeholders filled from `vars`.
 *
 * A key the current dictionary does not have comes back as the key itself —
 * deliberately ugly, so an untranslated string cannot hide behind the Chinese
 * one.
 */
export function t(key: string, vars?: Record<string, string | number>): string {
  const template = DICTIONARIES[current.value][key];
  if (template === undefined) {
    console.warn(`[i18n] missing key ${key} in ${current.value}`);
    return key;
  }
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (whole, name: string) =>
    name in vars ? String(vars[name]) : whole,
  );
}
