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
  "displays.inputHint": "这台机器占用屏幕时，显示器上的画面来自哪个输入源",
  "displays.readTitle": "读取这台显示器当前的输入源",
  "displays.readBusy": "正在问显示器…",
  "displays.readLong": "读取本机当前输入源",
  "displays.reading": "…",
  "displays.add": "＋ 添加显示器",
  "displays.scan": "扫描显示器",
  "displays.noneFound": "没有扫描到外接显示器。",
  "displays.readUnsupported": "这台显示器不回答输入源，请手动填写编号。",
  "displays.readNoMatch": "本机没有 EDID UUID 是「{uuid}」的显示器，请用「扫描显示器」核对这一行。",
  "displays.rowGone": "这一行已经不在了，读到的输入源没有写进去。",
  "displays.inputCode": "输入源 {code}",
  "displays.inputCustom": "自定义…",
  "displays.inputGroupCurrent": "现在显示的",
  "displays.inputGroupOther": "其它输入源",
  "displays.inputEmpty": "未选择",
  "displays.inputBackToList": "回到列表",

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

  // Moving a config between Macs. Every machine's file is the same but for
  // which host it is and where its trigger leaves to, so the second Mac reads
  // the first one's file instead of being typed out again.
  "transfer.title": "配置搬到另一台 Mac",
  "transfer.hint": "导出的是当前生效的配置，没保存的改动不在里面。",
  "transfer.export": "导出配置",
  "transfer.import": "导入配置",
  "transfer.rerun": "重新运行向导",

  // The wizard: the first screen a Mac that has never been configured shows,
  // and the fast path for a second one. Same words as the settings window —
  // 机器 / 通道 / 触发 / 跟随 / 输入源 — so the two cannot teach different ones.
  "wizard.title": "Relay 设置向导",
  "wizard.stepOf": "第 {step} 步 / 共 {total} 步",
  "wizard.back": "上一步",
  "wizard.next": "下一步",
  "wizard.finish": "完成",
  // 两个都是「离开向导」。从设置里点进来的有地方可退，第一次开机的没有，
  // 所以后者说的是去哪，不是退回哪。
  "wizard.cancel": "取消",
  "wizard.byHand": "我自己在设置里填",

  "wizard.choiceTitle": "开始之前",
  "wizard.choiceHint": "这是第一台配 Relay 的 Mac，还是已经有一台配好了？",
  "wizard.choiceFirst": "这是第一台",
  "wizard.choiceFirstHint": "四步：授权、键鼠、机器、显示器。",
  "wizard.choiceImport": "已经有一台配好了",
  "wizard.choiceImportHint": "读那台导出的配置文件，只问你一两句。",

  "wizard.importTitle": "从另一台 Mac 导入",
  "wizard.importHint": "两台机器的配置只差两处：本机是哪一台，以及离开本机时送去哪。",
  "wizard.importPick": "选择配置文件",
  "wizard.importChange": "换一个文件",
  "wizard.importNoFile": "还没有选文件",
  "wizard.importWhich": "哪一台是本机？",
  "wizard.keyboardSays": "键盘说本机是通道 {channel}",
  "wizard.leaveTitle": "从本机离开时，默认送去哪？",
  "wizard.leaveHint": "有三台机器，触发设备离开本机时无从推断该去哪一台。",

  "wizard.permissionTitle": "输入监控授权",
  "wizard.permissionHint": "没有这项授权，Relay 读不到键鼠的通道，也切不了。",
  "wizard.permissionRecheck": "重新检查",
  "wizard.permissionNeeded": "拿到这项授权才能继续",

  "wizard.devicesTitle": "键盘和鼠标",
  "wizard.devicesHint": "各选一个：「触发」留在本机负责发现切换，「跟随」跟着画面走。",
  "wizard.devicesNone":
    "没扫到可切换的设备。这一步可以往下走，但 Relay 要有一个键盘和一个鼠标才存得下配置：把键鼠切回本机再扫一次，或者自己在设置里填。",
  "wizard.thisChannel": "这台是通道 {channel}",
  "wizard.channelDisagree": "两个设备说的通道不一样，选一个：",
  "wizard.pickDevices": "触发和跟随各选一个",
  "wizard.pickChannel": "先选本机的通道",

  "wizard.machinesTitle": "共用这套键鼠的机器",
  "wizard.machinesHint": "名字随便起；通道号就是键鼠上的 Easy-Switch 按键。",

  "wizard.displaysTitle": "共用的显示器",
  "wizard.displaysHint": "选这几台机器轮流用的那台显示器。",
  "wizard.displaysNone": "没找到能用 DDC 控制的显示器，可以先不配，之后在设置里加。",
  "wizard.displaysSkip": "先不配显示器",
  "wizard.inputsTitle": "每台机器的输入源",
  "wizard.inputsLoading": "正在问显示器有哪些输入源…",
  "wizard.reading": "读取中…",

  "wizard.doneTitle": "配置好了",
  "wizard.doneHint": "配置已经写入，Relay 开始工作了。",
  "wizard.doneExportAsk": "要不要导出一份，拿去配别的机器？",
  "wizard.doneNext": "剩下的都能在设置里改。",
  "wizard.doneGo": "去设置页",

  // What the wizard checks before the backend ever sees the config, so the
  // user gets a sentence to act on instead of an English `ConfigError`.
  "wizard.needDevices": "Relay 需要一个「触发」设备和一个「跟随」设备，回到「键盘和鼠标」各选一个。",
  "wizard.duplicateChannel": "两台机器用了同一个通道号，每台各占一个。",
  "wizard.needLeaveTo": "有三台机器，先选离开本机时默认送去哪一台。",
  "wizard.needInput": "还没填「{name}」在这台显示器上的输入源。",
  "wizard.duplicateInput": "两台机器都填了输入源 {code}；一个输入源只能对上一台机器。",
  "wizard.errorRead": "无法读取这个配置文件：{detail}",
  "wizard.errorImport": "导入失败：{detail}",

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
  "toast.exported": "已导出",

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
  "error.export": "导出失败：{detail}",

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
  "displays.inputHint": "Which input source the monitor shows when that Mac owns the screen",
  "displays.readTitle": "Read this display's current input source",
  "displays.readBusy": "Asking the display…",
  "displays.readLong": "Read this Mac's input",
  "displays.reading": "…",
  "displays.add": "＋ Add a display",
  "displays.scan": "Scan displays",
  "displays.noneFound": "No external display found.",
  "displays.readUnsupported": "This display will not report its input source; type the number in.",
  "displays.readNoMatch":
    "No display on this Mac has the EDID UUID “{uuid}”; check this row with “Scan displays”.",
  "displays.rowGone": "That row is gone, so the input source that came back was not written.",
  "displays.inputCode": "Input {code}",
  "displays.inputCustom": "Custom…",
  "displays.inputGroupCurrent": "Showing now",
  "displays.inputGroupOther": "Other inputs",
  "displays.inputEmpty": "Not set",
  "displays.inputBackToList": "Back to the list",

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

  "transfer.title": "Move this config to another Mac",
  "transfer.hint":
    "What is exported is the config that is running; unsaved edits are not in it.",
  "transfer.export": "Export the config",
  "transfer.import": "Import a config",
  "transfer.rerun": "Run the wizard again",

  "wizard.title": "Set up Relay",
  "wizard.stepOf": "Step {step} of {total}",
  "wizard.back": "Back",
  "wizard.next": "Next",
  "wizard.finish": "Finish",
  // Both leave the wizard. The one opened from Settings has somewhere to go
  // back to; the one a new Mac starts in has not, so it says where it leads.
  "wizard.cancel": "Cancel",
  "wizard.byHand": "Set it up by hand instead",

  "wizard.choiceTitle": "Before we start",
  "wizard.choiceHint":
    "Is this the first Mac you are setting Relay up on, or is another one already set up?",
  "wizard.choiceFirst": "This is the first Mac",
  "wizard.choiceFirstHint": "Four steps: permission, keyboard and mouse, Macs, display.",
  "wizard.choiceImport": "Another Mac is already set up",
  "wizard.choiceImportHint": "Read the config it exported; you answer one or two questions.",

  "wizard.importTitle": "Import from another Mac",
  "wizard.importHint":
    "Two Macs' configs differ in two things only: which Mac this one is, and where a switch away from it goes.",
  "wizard.importPick": "Choose a config file",
  "wizard.importChange": "Choose another file",
  "wizard.importNoFile": "No file chosen yet",
  "wizard.importWhich": "Which of these is this Mac?",
  "wizard.keyboardSays": "The keyboard says this Mac is channel {channel}",
  "wizard.leaveTitle": "Where does a switch away from this Mac go?",
  "wizard.leaveHint":
    "With three Macs the trigger device cannot work out which one to leave to.",

  "wizard.permissionTitle": "Input Monitoring",
  "wizard.permissionHint":
    "Without it Relay cannot read the keyboard's channel, nor change it.",
  "wizard.permissionRecheck": "Check again",
  "wizard.permissionNeeded": "This step needs the permission before it can go on",

  "wizard.devicesTitle": "Keyboard and mouse",
  "wizard.devicesHint":
    "One of each: the trigger stays on this Mac and notices the switch, the follower goes with the screen.",
  "wizard.devicesNone":
    "No switchable device found. You can go on from here, but Relay needs one keyboard and one mouse before it can save: bring them back to this Mac and scan again, or set it up by hand.",
  "wizard.thisChannel": "This Mac is channel {channel}",
  "wizard.channelDisagree": "The two devices report different channels; pick one:",
  "wizard.pickDevices": "Pick one trigger and one follower",
  "wizard.pickChannel": "Pick this Mac's channel first",

  "wizard.machinesTitle": "The Macs sharing this keyboard",
  "wizard.machinesHint":
    "Any name will do; the channel is the Easy-Switch key on the keyboard and the mouse.",

  "wizard.displaysTitle": "The shared display",
  "wizard.displaysHint": "The one display these Macs take turns on.",
  "wizard.displaysNone":
    "No display this Mac can drive over DDC. You can leave it out and add one later in Settings.",
  "wizard.displaysSkip": "No shared display for now",
  "wizard.inputsTitle": "Each Mac's input source",
  "wizard.inputsLoading": "Asking the display which inputs it has…",
  "wizard.reading": "Reading…",

  "wizard.doneTitle": "All set",
  "wizard.doneHint": "The config is written and Relay is running on it.",
  "wizard.doneExportAsk": "Export a copy to set up the next Mac with?",
  "wizard.doneNext": "Everything else is in Settings.",
  "wizard.doneGo": "Open Settings",

  "wizard.needDevices":
    "Relay needs one trigger device and one follower; go back to “Keyboard and mouse” and pick one of each.",
  "wizard.duplicateChannel": "Two Macs claim the same channel; each one needs its own.",
  "wizard.needLeaveTo": "With three Macs, pick where a switch away from this one goes.",
  "wizard.needInput": "“{name}” has no input source on this display yet.",
  "wizard.duplicateInput":
    "Two Macs are both on input source {code}; one input source can name only one Mac.",
  "wizard.errorRead": "Cannot read that config file: {detail}",
  "wizard.errorImport": "Importing failed: {detail}",

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
  "toast.exported": "Exported",

  "error.loadConfig": "Cannot read the config file: {detail}",
  "error.save": "Saving failed: {detail}",
  "error.loadPermission": "Cannot read the Input Monitoring state: {detail}",
  "error.requestPermission": "Cannot ask for Input Monitoring: {detail}",
  "error.openSettings": "Cannot open System Settings: {detail}",
  "error.scanDisplays": "Scanning displays failed: {detail}",
  "error.readInput": "Reading the input source failed: {detail}",
  "error.scanDevices": "Scanning devices failed: {detail}",
  "error.switch": "The switch failed: {detail}",
  "error.export": "Exporting failed: {detail}",

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
