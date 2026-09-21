<script setup lang="ts">
// The whole config.json, editable, in four tabs: hosts, displays, devices,
// timing and which host this machine is. Saving hands the file to the backend,
// which validates it before writing; the core picks the new file up through its
// watcher.
import { getCurrentWindow } from "@tauri-apps/api/window";
import { computed, nextTick, onMounted, onUnmounted, ref, watchEffect } from "vue";

import { setLanguage, t } from "../lib/i18n";
import * as ipc from "../lib/ipc";
import type {
  Config,
  DeviceConfig,
  DeviceRole,
  DiscoveredDevice,
  DiscoveredDisplay,
  DisplayConfig,
  Host,
  StepResult,
  Status,
} from "../lib/ipc";

const TABS = [
  { id: "overview", key: "tab.overview" },
  { id: "machines", key: "tab.machines" },
  { id: "devices", key: "tab.devices" },
  { id: "advanced", key: "tab.advanced" },
] as const;

type TabId = (typeof TABS)[number]["id"];

const ROLE_KEYS: Record<DeviceRole, string> = {
  keyboard: "devices.roleKeyboard",
  mouse: "devices.roleMouse",
  other: "devices.roleOther",
};

const TRANSPORTS = ["ble", "bolt", "unifying"];

/** `Status.state` to its short label; an unknown state shows the core's word. */
const STATE_KEYS: Record<string, string> = {
  Idle: "state.idle",
  Confirming: "state.confirming",
  Switching: "state.switching",
  Cooldown: "state.cooldown",
  Unconfigured: "state.unconfigured",
};

/** The states in which something is under way rather than settled. */
const BUSY_STATES = ["Confirming", "Switching", "Cooldown"];

const cfg = ref<Config | null>(null);
const status = ref<Status | null>(null);
const granted = ref(false);
const tab = ref<TabId>("overview");
/** The top banner: a load failure or the backend's validation message. */
const banner = ref("");
const bannerEl = ref<HTMLElement | null>(null);
/** The same message as the banner, repeated next to the save button. */
const saveError = ref("");
const toast = ref("");
const saving = ref(false);
/** The inline device scan result; `null` while the list is closed. */
const found = ref<DiscoveredDevice[] | null>(null);
/** `scan_devices` probes every HID++ node and takes seconds. */
const scanning = ref(false);
/** The inline display scan result; `null` while the list is closed. */
const foundDisplays = ref<DiscoveredDisplay[] | null>(null);
const scanningDisplays = ref(false);
/** The display row whose read button is waiting on the monitor. */
const readingInput = ref<number | null>(null);
/** The `vid:pid` of every Logitech device attached to this Mac right now. */
const hereIds = ref<Set<string>>(new Set());
/** The overview's own read, which asks the same monitor over the same cable. */
const readingScreen = ref(false);
/** Which host the screen is on, or `null` when the monitor would not say. */
const screenAt = ref<number | null>(null);
const switching = ref<number | null>(null);

/**
 * When the status is re-read after a save.
 *
 * The config watcher settles a few hundred ms after the file lands, so a
 * language just chosen shows up on one of these rather than on the
 * `refreshStatus()` the save itself does. A handful of reads over a second and
 * a half is enough; this window runs no timer of its own otherwise.
 */
const SAVE_REFRESH_MS = [300, 700, 1200, 1600];

let toastTimer = 0;
let saveTimers: number[] = [];

// The native title bar is outside the Vue tree, so the window title has to be
// set from here; `t()` reads the language ref, so this re-runs whenever the
// language changes while the window is open. `windows.rs` gives the window a
// neutral title for the moment before this view loads.
watchEffect(() => {
  const title = t("app.title");
  getCurrentWindow()
    .setTitle(title)
    .catch(() => {
      /* a title the window manager refused is not worth a banner */
    });
});

onMounted(() => void load());

onUnmounted(() => {
  window.clearTimeout(toastTimer);
  for (const timer of saveTimers) window.clearTimeout(timer);
});

async function load() {
  // A reload replaces the device and display rows, so the scan lists — whose
  // "already added" state is read off those rows — must not outlive them.
  found.value = null;
  foundDisplays.value = null;
  try {
    cfg.value = await ipc.getConfig();
    banner.value = "";
  } catch (err) {
    await showBanner(t("error.loadConfig", { detail: String(err) }));
  }
  await refreshStatus();
  try {
    granted.value = await ipc.inputMonitoringGranted();
  } catch (err) {
    // A config error, when there is one, is the more useful of the two.
    if (!banner.value) await showBanner(t("error.loadPermission", { detail: String(err) }));
  }
  await refreshHere();
  await readScreen();
}

/**
 * Re-reads which switchable devices are attached to this Mac.
 *
 * The IORegistry walk behind `list_hid_devices` needs no permission and takes
 * milliseconds, so it runs with the rest of the overview — on mount, when that
 * tab comes up and after a manual switch — and never on a timer. A failure
 * empties the set, which only costs the rows their "on this Mac" answer.
 */
async function refreshHere() {
  try {
    const devices = await ipc.listHidDevices();
    hereIds.value = new Set(devices.map((device) => device.id.toLowerCase()));
  } catch {
    hereIds.value = new Set();
  }
}

/** Shows `message` at the top and scrolls it back into view. */
async function showBanner(message: string) {
  banner.value = message;
  await nextTick();
  if (bannerEl.value) bannerEl.value.scrollIntoView({ block: "start" });
}

async function refreshStatus() {
  try {
    status.value = await ipc.getStatus();
    // The core resolved `options.language: "auto"` for us, and the tray reads
    // the very same field, so the two can never drift apart.
    setLanguage(status.value.language);
  } catch {
    status.value = null;
  }
}

/**
 * Re-reads the status a few times over the next second and a half, which is
 * how a language chosen in this window reaches it: the save returns before the
 * core has reloaded the file.
 */
function refreshStatusAfterSave() {
  for (const timer of saveTimers) window.clearTimeout(timer);
  saveTimers = SAVE_REFRESH_MS.map((delay) =>
    window.setTimeout(() => void refreshStatus(), delay),
  );
}

function selectTab(id: TabId) {
  tab.value = id;
  void refreshStatus();
  // Only the overview shows where the screen and the devices are, and asking
  // the monitor costs a DDC round trip, so both are asked when that tab comes
  // up — never on a timer.
  if (id === "overview") {
    void refreshHere();
    void readScreen();
  }
}

/**
 * Asks the first configured display which input it is showing and turns that
 * into a host.
 *
 * The answer is this Mac's own cable, so an input that matches this host's code
 * means the screen is here; one that matches another host's code means it is
 * there. Anything else — no display configured, a monitor that will not answer,
 * a code nobody claims — is simply unknown.
 */
async function readScreen() {
  const c = cfg.value;
  if (!c || c.displays.length === 0) {
    screenAt.value = null;
    return;
  }
  // The row's own read button talks to the same monitor; one at a time.
  if (readingScreen.value || readingInput.value !== null) return;
  // So does the core, mid-switch, over the same cable: a read dropped into its
  // VCP traffic can confuse either side. The last answer stays on screen until
  // the next read, which `switchTo()` runs once the switch is through.
  const state = status.value?.state;
  if (state === "Switching" || state === "Confirming") return;
  readingScreen.value = true;
  const display = c.displays[0];
  try {
    const code = await ipc.readDisplayInput(display.edid_uuid);
    const rows = cfg.value?.displays ?? [];
    // The config may have been reloaded while the monitor answered.
    const row =
      rows.find(
        (other) => other.edid_uuid.toLowerCase() === display.edid_uuid.toLowerCase(),
      ) ?? null;
    const owner =
      code === null || row === null
        ? undefined
        : (cfg.value?.hosts ?? []).find((host) => row.input_by_host[String(host.index)] === code);
    screenAt.value = owner ? owner.index : null;
  } catch {
    screenAt.value = null;
  } finally {
    readingScreen.value = false;
  }
}

function showToast(message: string) {
  toast.value = message;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (toast.value = ""), 2500);
}

/** A whole number in `[min, max]`; an emptied number field holds `""` instead. */
function inRange(value: unknown, min: number, max: number): boolean {
  return typeof value === "number" && Number.isInteger(value) && value >= min && value <= max;
}

/** Mirrors `MIN_COOLDOWN_MS` in `crates/relay-core/src/config.rs`. */
const MIN_COOLDOWN_MS = 1000;
/** Mirrors `MIN_DEBOUNCE_MS` in `crates/relay-core/src/config.rs`. */
const MIN_DEBOUNCE_MS = 100;

/**
 * The shapes the backend cannot report on, because a blank or out-of-range
 * number field fails to deserialize before `validate()` ever sees it. Everything
 * else is still the backend's to judge.
 */
function shapeProblem(c: Config): string {
  for (const [i, host] of c.hosts.entries()) {
    if (!inRange(host.index, 0, 2)) return t("validate.hostChannel", { row: i + 1 });
  }
  for (const display of c.displays) {
    for (const [host, input] of Object.entries(display.input_by_host)) {
      if (!inRange(input, 0, 255)) {
        const name = display.name || t("common.unnamed");
        return t("validate.displayInput", { name, channel: Number(host) + 1 });
      }
    }
  }
  const { debounce_ms, cooldown_ms, ddc_retries } = c.timing;
  const big = Number.MAX_SAFE_INTEGER;
  // The lower bounds mirror MIN_DEBOUNCE_MS / MIN_COOLDOWN_MS in relay-core:
  // a near-zero cooldown lets repeated ChangeHost calls corrupt the link.
  if (!inRange(debounce_ms, MIN_DEBOUNCE_MS, big))
    return t("validate.debounce", { ms: MIN_DEBOUNCE_MS });
  if (!inRange(cooldown_ms, MIN_COOLDOWN_MS, big))
    return t("validate.cooldown", { ms: MIN_COOLDOWN_MS });
  if (!inRange(ddc_retries, 0, 255)) return t("validate.retries");
  return "";
}

async function save() {
  const c = cfg.value;
  if (!c) return;
  const problem = shapeProblem(c);
  if (problem) {
    saveError.value = problem;
    await showBanner(problem);
    return;
  }
  saving.value = true;
  try {
    await ipc.saveConfig(c);
    // The validation message is the only thing the banner ever shows for a
    // save, so an accepted one clears it.
    banner.value = "";
    saveError.value = "";
    showToast(t("toast.saved"));
    await refreshStatus();
    refreshStatusAfterSave();
  } catch (err) {
    // The backend's `ConfigError` is still English, so it goes in as the detail
    // of a localized heading — the same shape the load path uses.
    const message = t("error.save", { detail: String(err) });
    saveError.value = message;
    await showBanner(message);
  } finally {
    saving.value = false;
  }
}

async function requestPermission() {
  try {
    granted.value = await ipc.requestInputMonitoring();
  } catch (err) {
    await showBanner(t("error.requestPermission", { detail: String(err) }));
  }
  await refreshStatus();
}

async function openPrivacySettings() {
  try {
    await ipc.openPrivacySettings();
  } catch (err) {
    await showBanner(t("error.openSettings", { detail: String(err) }));
  }
}

function hostName(host: Host): string {
  return host.name || t("common.unnamed");
}

/** The name of the host in `index`, or its channel when no such host exists. */
function hostNameAt(index: number | null): string {
  if (index === null) return t("common.unknown");
  const host = cfg.value?.hosts.find((other) => other.index === index);
  return host ? hostName(host) : t("overview.channelOnly", { channel: index + 1 });
}

function hostLabel(host: Host): string {
  return t("hosts.label", { name: hostName(host), channel: host.index + 1 });
}

const thisHost = computed(() => cfg.value?.hosts.find((host) => host.index === cfg.value?.this_host));

/** This Mac's own input code on the first configured display, if it has one. */
const thisInput = computed(() => {
  const c = cfg.value;
  if (!c || c.displays.length === 0) return undefined;
  return c.displays[0].input_by_host[String(c.this_host)];
});

const stateLabel = computed(() => {
  const state = status.value?.state;
  if (!state) return t("common.unknown");
  const key = STATE_KEYS[state];
  return key ? t(key) : state;
});

/** Green unless something needs the user, amber while a switch is under way. */
const toneClass = computed(() => {
  const s = status.value;
  if (!s || !s.config_ok || !granted.value) return "bad";
  return BUSY_STATES.includes(s.state) ? "warn" : "";
});

/** The header's second half: where the screen is, in one phrase. */
const screenLabel = computed(() => {
  if (screenAt.value === null) return t("overview.screenUnknown");
  if (screenAt.value === cfg.value?.this_host) return t("overview.screenHere");
  return t("overview.screenOn", { name: hostNameAt(screenAt.value) });
});

/** The same thing as a value in the overview's screen row. */
const screenWhere = computed(() => {
  if (screenAt.value === null) return t("common.unknown");
  if (screenAt.value === cfg.value?.this_host) return t("overview.here");
  return hostNameAt(screenAt.value);
});

const heroDetail = computed(() => {
  const host = thisHost.value;
  if (!host) return t("common.unknown");
  const channel = host.index + 1;
  const input = thisInput.value;
  return input === undefined
    ? t("overview.channelOnly", { channel })
    : t("overview.channelInput", { channel, input });
});

const lastReport = computed(() => status.value?.last_report ?? null);

/** A step that was skipped is neither a success to celebrate nor a failure. */
function stepClass(step: StepResult): string {
  if (!step.ok) return "bad";
  return step.detail.includes("skipped") ? "muted" : "ok";
}

/** `text` without `prefix`, or `null` when it does not start with it. */
function after(text: string, prefix: string): string | null {
  return text.startsWith(prefix) ? text.slice(prefix.length) : null;
}

/**
 * Turns the step name the core wrote into this window's language.
 *
 * `relay-core` writes every step in English on purpose — the same strings go
 * to the log and to the CLI — so the translating happens here, and only for
 * the shapes `executor.rs` actually produces. Anything else is shown word for
 * word rather than guessed at.
 */
function stepWhat(step: StepResult): string {
  const display = after(step.what, "display ");
  if (display !== null) return t("step.display", { name: display });
  const device = after(step.what, "device ");
  if (device !== null) return t("step.device", { name: device });
  return step.what;
}

/** The step details that carry no number, by the exact word the core writes. */
const STEP_DETAIL_KEYS: Record<string, string> = {
  "already at target": "step.alreadyAtTarget",
  "not on this Mac, skipped": "step.skipped",
  "device not present": "step.deviceNotPresent",
  "display not found": "step.displayNotFound",
  "device not found": "step.deviceNotFound",
  "display did not answer in time": "step.displayTimeout",
  "device did not answer in time": "step.deviceTimeout",
};

/** `target {n} is beyond the device's {m} hosts`, both numbers captured. */
const BEYOND = /^target (\d+) is beyond the device's (\d+) hosts$/;

/**
 * One step's outcome in this window's language, or unchanged.
 *
 * A detail that matches none of the shapes above is arbitrary text from the
 * DDC or HID++ layer, and swallowing it would hide the one thing a failed
 * switch has to say — so it is passed straight through.
 */
function stepDetailText(detail: string): string {
  const fixed = STEP_DETAIL_KEYS[detail];
  if (fixed) return t(fixed);

  const input = after(detail, "input ");
  if (input !== null && /^\d+$/.test(input)) return t("step.input", { input });

  // The core writes the raw host index; hosts are named by it, and an unnamed
  // one falls back to the channel label the rest of the window uses.
  const host = after(detail, "switched to host ");
  if (host !== null && /^\d+$/.test(host)) {
    return t("step.switched", { name: hostNameAt(Number(host)) });
  }

  const beyond = BEYOND.exec(detail);
  if (beyond) {
    return t("step.beyond", { channel: Number(beyond[1]) + 1, count: beyond[2] });
  }

  return detail;
}

/** The outcome and, appended, the timing the window owns. */
function stepDetail(step: StepResult): string {
  return `${stepDetailText(step.detail)} · ${step.ms} ms`;
}

function deviceName(device: DeviceConfig): string {
  return device.name || t("common.unnamed");
}

/**
 * Where a configured device is, as far as this window can tell.
 *
 * This Mac can simply look: a device on the HID bus here is here, whatever the
 * last switch did. Only when it is absent does the last report get a say, and
 * then the answer is where the device was last sent — a memory, not a reading,
 * so it is worded as one. Anything else is unknown.
 */
function deviceLocation(device: DeviceConfig): string {
  if (device.id && hereIds.value.has(device.id.toLowerCase())) return t("overview.here");
  // The executor names the step after the device, falling back to its id when
  // the core has no device by that id at all.
  const step = lastReport.value?.steps.find(
    (other) => other.what === `device ${device.name}` || other.what === `device ${device.id}`,
  );
  const host = step?.ok ? after(step.detail, "switched to host ") : null;
  if (host !== null && /^\d+$/.test(host)) {
    return t("overview.lastSeen", { name: hostNameAt(Number(host)) });
  }
  return t("common.unknown");
}

/**
 * The hosts table edits `index + 1`, like every other channel label. Renumbering a
 * host carries along everything that points at the old slot, so no display
 * input or `leave_to` is left behind.
 */
function setHostIndex(host: Host, raw: string, event: Event) {
  const c = cfg.value;
  const value = Number(raw);
  if (!c || raw === "" || !Number.isInteger(value)) return;
  const previous = host.index;
  const next = value - 1;
  if (next === previous) return;

  const target = event.target as HTMLInputElement;

  // Guard 1: only channels 1-3 (index 0-2) exist. Without this, typing 0 or 4
  // would store host.index as -1 or 3, an out-of-bounds slot nothing else
  // expects. Reject and re-sync the field back to the current value instead
  // of mutating the model.
  if (next < 0 || next > 2) {
    target.value = String(host.index + 1);
    showToast(t("toast.channelRange"));
    return;
  }

  // Guard 2: refuse to retarget onto a slot another host already owns.
  // Migrating would silently overwrite that host's input_by_host entries and
  // any device leave_to references pointing at it, losing data. Refusing is
  // simpler and safer than trying to swap the two hosts' slots.
  const owner = c.hosts.find((other) => other !== host && other.index === next);
  if (owner) {
    target.value = String(host.index + 1);
    showToast(t("toast.channelTaken", { channel: next + 1, name: owner.name }));
    return;
  }

  host.index = next;
  if (c.this_host === previous) c.this_host = next;
  for (const display of c.displays) {
    const input = display.input_by_host[String(previous)];
    if (input !== undefined) {
      delete display.input_by_host[String(previous)];
      display.input_by_host[String(next)] = input;
    }
  }
  for (const device of c.devices) {
    if (device.leave_to === previous) device.leave_to = next;
  }
}

/**
 * Slot 0 is the one a two-Mac setup usually leaves unpaired, so only the very
 * first host may claim it; later ones take the lowest free slot from 1 up.
 * `null` means all three slots are taken.
 */
function freeHostSlot(): number | null {
  const c = cfg.value;
  if (!c) return null;
  const used = new Set(c.hosts.map((host) => host.index));
  const order = c.hosts.length === 0 ? [0, 1, 2] : [1, 2, 0];
  return order.find((index) => !used.has(index)) ?? null;
}

const canAddHost = computed(() => freeHostSlot() !== null);

function addHost() {
  const c = cfg.value;
  const free = freeHostSlot();
  if (!c || free === null) return;
  c.hosts.push({ index: free, name: t("hosts.newName", { channel: free + 1 }) });
}

function removeHost(position: number) {
  const c = cfg.value;
  if (!c) return;
  const [removed] = c.hosts.splice(position, 1);
  // `this_host` must keep pointing at a declared host, or the save is rejected.
  if (removed && removed.index === c.this_host && c.hosts.length > 0) {
    c.this_host = c.hosts[0].index;
  }
}

function addDisplay() {
  cfg.value?.displays.push({ edid_uuid: "", name: "", input_by_host: {} });
}

function removeDisplay(position: number) {
  cfg.value?.displays.splice(position, 1);
}

async function toggleDisplayScan() {
  if (foundDisplays.value) {
    foundDisplays.value = null;
    return;
  }
  scanningDisplays.value = true;
  try {
    foundDisplays.value = await ipc.listDisplays();
  } catch (err) {
    await showBanner(t("error.scanDisplays", { detail: String(err) }));
  } finally {
    scanningDisplays.value = false;
  }
}

/**
 * A scanned display is already configured when some row carries the same
 * `edid_uuid` — a second row for it would only write the same DDC packet
 * twice. The registry prints UUIDs upper case while a hand-edited config may
 * not, so the comparison ignores case, exactly as the backend's `pick` does.
 */
function displayAlreadyAdded(display: DiscoveredDisplay): boolean {
  const rows = cfg.value?.displays ?? [];
  return rows.some((row) => row.edid_uuid.toLowerCase() === display.edid_uuid.toLowerCase());
}

function addDisplayFromScan(display: DiscoveredDisplay) {
  if (displayAlreadyAdded(display)) return;
  cfg.value?.displays.push({
    edid_uuid: display.edid_uuid,
    name: display.name,
    input_by_host: {},
  });
}

/**
 * Which of the two things a `null` from `read_display_input` meant.
 *
 * The command answers `null` both when the monitor will not report its input
 * source and when no display on this Mac carries that EDID UUID — and the fix
 * for the second is to edit the config, not the monitor. Walking the registry
 * is cheap and needs no permission, so ask it which happened; if even that
 * fails, keep the wording that blames the monitor.
 */
async function readFailure(uuid: string): Promise<string> {
  // An empty UUID means "the first external display", so there is no UUID to
  // mismatch: whatever is there did not answer.
  if (uuid === "") return t("displays.readUnsupported");
  try {
    const displays = await ipc.listDisplays();
    const here = displays.some((other) => other.edid_uuid.toLowerCase() === uuid.toLowerCase());
    if (!here) return t("displays.readNoMatch", { uuid });
  } catch {
    /* The registry walk failing says nothing about the monitor. */
  }
  return t("displays.readUnsupported");
}

/**
 * Asks the monitor which input source it is showing and fills that cell in.
 *
 * Only this machine's column offers it: DDC reaches the display over this
 * Mac's own cable, so the answer is this host's input code and nobody else's.
 * Nothing is saved — the user still presses the save button.
 */
async function readInput(display: DisplayConfig, host: number, row: number) {
  // One read at a time: `readingInput` holds a single row, so a second one
  // starting would hand the first row's button back mid-flight. The overview's
  // own read shares the cable and the same rule.
  if (readingInput.value !== null || readingScreen.value) return;
  readingInput.value = row;
  // A failure from the last read must not sit above this one's result.
  banner.value = "";
  const uuid = display.edid_uuid;
  try {
    const code = await ipc.readDisplayInput(uuid);
    if (code === null) {
      await showBanner(await readFailure(uuid));
      return;
    }
    // The read is asynchronous, and a config reload may have replaced every row
    // meanwhile, so the answer goes to whichever row now carries that UUID
    // rather than to the object the click captured. (The row's own delete stays
    // disabled while its read is in flight, so the usual case is a reload.)
    const target = cfg.value?.displays.find(
      (other) => other.edid_uuid.toLowerCase() === uuid.toLowerCase(),
    );
    if (!target) {
      await showBanner(t("displays.rowGone"));
      return;
    }
    target.input_by_host[String(host)] = code;
    showToast(t("toast.readInput", { code }));
  } catch (err) {
    await showBanner(t("error.readInput", { detail: String(err) }));
  } finally {
    readingInput.value = null;
  }
}

function inputFor(display: DisplayConfig, host: number): number | "" {
  return display.input_by_host[String(host)] ?? "";
}

function setInput(display: DisplayConfig, host: number, raw: string) {
  const value = Number(raw);
  if (raw === "" || Number.isNaN(value)) {
    delete display.input_by_host[String(host)];
  } else {
    display.input_by_host[String(host)] = Math.trunc(value);
  }
}

function addDevice(device?: Partial<DeviceConfig>) {
  cfg.value?.devices.push({
    id: "",
    name: "",
    role: "keyboard",
    transport: "ble",
    serial: null,
    is_trigger: false,
    follow: false,
    leave_to: null,
    ...device,
  });
}

function removeDevice(position: number) {
  cfg.value?.devices.splice(position, 1);
}

/**
 * Trigger and follow are exclusive per device — `Config::validate()` rejects a row
 * with both — so ticking one unticks the other. `v-model` has already written
 * the box the user clicked; this only clears its counterpart.
 */
function setRole(device: DeviceConfig, checked: "is_trigger" | "follow") {
  if (checked === "is_trigger") {
    if (device.is_trigger) device.follow = false;
  } else if (device.follow) {
    device.is_trigger = false;
  }
}

function setLeaveTo(device: DeviceConfig, raw: string) {
  device.leave_to = raw === "" ? null : Number(raw);
}

async function toggleScan() {
  if (found.value) {
    found.value = null;
    return;
  }
  scanning.value = true;
  try {
    found.value = await ipc.scanDevices();
  } catch (err) {
    await showBanner(t("error.scanDevices", { detail: String(err) }));
  } finally {
    scanning.value = false;
  }
}

function roleLabel(role: DeviceRole): string {
  const key = ROLE_KEYS[role];
  return key ? t(key) : role;
}

/** The scanned serial, which only ever comes from the device itself. */
function serialLabel(device: DeviceConfig): string {
  return device.serial ? t("devices.serial", { serial: device.serial }) : t("devices.noSerial");
}

/**
 * A scanned device is already configured when some row carries the same id —
 * the serial does not enter into it. `Config::validate()` rejects two rows
 * sharing an id (the plan carries a device as its id alone, so the second row
 * would never be reached), so a second row for an id already present is a
 * config the backend would refuse to save.
 */
function alreadyAdded(device: DiscoveredDevice): boolean {
  const rows = cfg.value?.devices ?? [];
  return rows.some((row) => row.id === device.id);
}

function addFromScan(device: DiscoveredDevice) {
  if (alreadyAdded(device)) return;
  addDevice({
    id: device.id,
    serial: device.serial,
    name: device.name,
    role: device.role_guess,
    transport: "ble",
    is_trigger: false,
    follow: true,
  });
}

/**
 * A switch started by hand, skipping the debounce. The row for this Mac is the
 * rescue that pulls the screen and the follow devices back here, exactly like
 * the tray menu's entry for it, so it stays enabled.
 */
async function switchTo(host: Host) {
  switching.value = host.index;
  try {
    await ipc.triggerSwitch(host.index);
    banner.value = "";
  } catch (err) {
    await showBanner(t("error.switch", { detail: String(err) }));
  } finally {
    switching.value = null;
    // The report of this very switch is on the status, and the screen and the
    // devices have just moved, so all three are worth re-reading.
    await refreshStatus();
    await refreshHere();
    await readScreen();
  }
}
</script>

<template>
  <div class="win">
    <header>
      <div class="title">
        <b>{{ t("app.title") }}</b>
        <span class="state">
          <span class="dot" :class="toneClass"></span>
          {{ stateLabel }} · {{ screenLabel }}
        </span>
      </div>
      <nav class="seg">
        <button
          v-for="entry in TABS"
          :key="entry.id"
          :class="{ on: tab === entry.id }"
          @click="selectTab(entry.id)"
        >
          {{ t(entry.key) }}
        </button>
      </nav>
    </header>

    <main>
      <p v-if="banner" ref="bannerEl" class="banner">{{ banner }}</p>

      <template v-if="tab === 'overview'">
        <section class="card">
          <h2>{{ t("overview.thisMac") }}</h2>
          <p class="hero">
            <b>{{ thisHost ? hostName(thisHost) : t("common.unknown") }}</b>
            <span class="muted">{{ heroDetail }}</span>
            <span class="pill" :class="toneClass">{{ stateLabel }}</span>
          </p>
          <div v-if="status && !status.config_ok" class="row">
            <span class="bad">
              {{
                status.config_error
                  ? t("overview.configInactiveDetail", { detail: status.config_error })
                  : t("overview.configInactive")
              }}
            </span>
          </div>
          <div class="row">
            <span>{{ t("overview.screen") }}</span>
            <span class="v">{{ screenWhere }}</span>
          </div>
          <div v-for="(device, i) in cfg?.devices ?? []" :key="i" class="row">
            <span>
              {{ deviceName(device) }}<span class="muted"> · {{ roleLabel(device.role) }}</span>
            </span>
            <span class="v">{{ deviceLocation(device) }}</span>
          </div>
          <div class="row">
            <span>{{ t("permission.label") }}</span>
            <span v-if="granted" class="v ok">{{ t("permission.granted") }}</span>
            <span v-else class="v">
              <span class="bad">{{ t("permission.denied") }}</span>
              <button class="mini" @click="requestPermission">{{ t("permission.request") }}</button>
              <button class="mini" @click="openPrivacySettings">
                {{ t("permission.openSettings") }}
              </button>
            </span>
          </div>
        </section>

        <section v-if="cfg" class="card">
          <h2>{{ t("overview.manual") }}</h2>
          <p v-if="cfg.hosts.length === 0" class="sub">{{ t("overview.noHosts") }}</p>
          <div v-for="host in cfg.hosts" :key="host.index" class="row">
            <span>
              {{ hostName(host) }}
              <span class="muted">
                ·
                {{
                  host.index === cfg.this_host
                    ? t("overview.thisMac")
                    : t("overview.channelOnly", { channel: host.index + 1 })
                }}
              </span>
            </span>
            <span class="v">
              <span v-if="switching === host.index">{{ t("overview.switching") }}</span>
              <button class="mini" :disabled="switching !== null" @click="switchTo(host)">
                {{
                  host.index === cfg.this_host ? t("overview.pullBack") : t("overview.switchTo")
                }}
              </button>
            </span>
          </div>
        </section>

        <section v-if="lastReport" class="card">
          <h2>
            {{ t("overview.lastSwitch", { host: hostNameAt(lastReport.target) }) }}
            <span v-if="!lastReport.ok" class="bad">{{ t("overview.someStepsFailed") }}</span>
          </h2>
          <div v-for="(step, i) in lastReport.steps" :key="i" class="row">
            <span class="what">{{ stepWhat(step) }}</span>
            <span class="v" :class="stepClass(step)">{{ stepDetail(step) }}</span>
          </div>
        </section>
      </template>

      <template v-if="tab === 'machines' && cfg">
        <section class="card">
          <h2>{{ t("hosts.title") }}</h2>
          <p class="sub">{{ t("hosts.channelHint") }}</p>
          <table>
            <thead>
              <tr>
                <th>{{ t("common.name") }}</th>
                <th style="width: 64px">{{ t("hosts.channel") }}</th>
                <th class="center" style="width: 48px">{{ t("hosts.thisMac") }}</th>
                <th style="width: 30px"></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(host, i) in cfg.hosts" :key="i">
                <td><input v-model="host.name" type="text" /></td>
                <td class="num">
                  <input
                    type="number"
                    min="1"
                    max="3"
                    :value="host.index + 1"
                    @input="setHostIndex(host, ($event.target as HTMLInputElement).value, $event)"
                    @blur="($event.target as HTMLInputElement).value = String(host.index + 1)"
                  />
                </td>
                <td class="center">
                  <input v-model="cfg.this_host" type="radio" :value="host.index" />
                </td>
                <td class="center">
                  <button class="mini" :title="t('common.delete')" @click="removeHost(i)">−</button>
                </td>
              </tr>
            </tbody>
          </table>
          <button class="link" :disabled="!canAddHost" @click="addHost">{{ t("hosts.add") }}</button>
        </section>

        <section class="card">
          <h2>{{ t("displays.title") }}</h2>
          <div class="scroll">
            <table>
              <thead>
                <tr>
                  <th>{{ t("displays.nameColumn") }}</th>
                  <th
                    v-for="host in cfg.hosts"
                    :key="host.index"
                    :title="t('displays.inputHint')"
                    :style="{ width: host.index === cfg.this_host ? '108px' : '66px' }"
                  >
                    {{ hostName(host) }}
                  </th>
                  <th style="width: 30px"></th>
                </tr>
              </thead>
              <tbody>
                <template v-for="(display, i) in cfg.displays" :key="i">
                  <tr>
                    <td><input v-model="display.name" type="text" /></td>
                    <td v-for="host in cfg.hosts" :key="host.index">
                      <div class="input-cell">
                        <input
                          type="number"
                          min="0"
                          max="255"
                          :value="inputFor(display, host.index)"
                          @input="
                            setInput(display, host.index, ($event.target as HTMLInputElement).value)
                          "
                        />
                        <button
                          v-if="host.index === cfg.this_host"
                          class="mini"
                          :title="t('displays.readTitle')"
                          :disabled="readingInput !== null || readingScreen"
                          @click="readInput(display, host.index, i)"
                        >
                          {{ readingInput === i ? t("displays.reading") : t("displays.read") }}
                        </button>
                      </div>
                    </td>
                    <td class="center">
                      <button
                        class="mini"
                        :title="t('common.delete')"
                        :disabled="readingInput === i"
                        @click="removeDisplay(i)"
                      >
                        −
                      </button>
                    </td>
                  </tr>
                  <tr class="meta">
                    <td :colspan="cfg.hosts.length + 2">
                      <span class="muted">{{ t("displays.edid") }}</span>
                      <input
                        v-model="display.edid_uuid"
                        type="text"
                        class="edid"
                        :title="t('displays.edidHint')"
                      />
                    </td>
                  </tr>
                </template>
              </tbody>
            </table>
          </div>
          <button class="link" @click="addDisplay">{{ t("displays.add") }}</button>
          <button class="link" :disabled="scanningDisplays" @click="toggleDisplayScan">
            {{
              foundDisplays
                ? t("scan.collapse")
                : scanningDisplays
                  ? t("scan.scanning")
                  : t("displays.scan")
            }}
          </button>
          <div v-if="foundDisplays" class="found">
            <p v-if="foundDisplays.length === 0" class="muted empty">
              {{ t("displays.noneFound") }}
            </p>
            <div
              v-for="(display, i) in foundDisplays"
              :key="i"
              class="found-row"
              :class="{ dim: displayAlreadyAdded(display) }"
            >
              <span class="found-name">{{ display.name }}</span>
              <code>{{ display.edid_uuid }}</code>
              <button
                class="mini"
                :disabled="displayAlreadyAdded(display)"
                @click="addDisplayFromScan(display)"
              >
                {{ t("common.add") }}
              </button>
            </div>
          </div>
        </section>
      </template>

      <template v-if="tab === 'devices' && cfg">
        <section class="card">
          <h2>{{ t("permission.title") }}</h2>
          <div class="row">
            <span>{{ t("permission.label") }}</span>
            <span v-if="granted" class="v ok">{{ t("permission.granted") }}</span>
            <span v-else class="v">
              <span class="bad">{{ t("permission.denied") }}</span>
              <button class="mini" @click="requestPermission">{{ t("permission.request") }}</button>
              <button class="mini" @click="openPrivacySettings">
                {{ t("permission.openSettings") }}
              </button>
            </span>
          </div>
        </section>

        <section class="card">
          <h2>{{ t("devices.title") }}</h2>
          <div class="scroll">
            <table>
              <thead>
                <tr>
                  <th>{{ t("common.name") }}</th>
                  <th class="center" style="width: 44px" :title="t('devices.triggerTitle')">
                    {{ t("devices.trigger") }}
                  </th>
                  <th class="center" style="width: 44px" :title="t('devices.followTitle')">
                    {{ t("devices.follow") }}
                  </th>
                  <th style="width: 104px" :title="t('devices.leaveToTitle')">
                    {{ t("devices.leaveTo") }}
                  </th>
                  <th style="width: 30px"></th>
                </tr>
              </thead>
              <tbody>
                <template v-for="(device, i) in cfg.devices" :key="i">
                  <tr>
                    <td><input v-model="device.name" type="text" /></td>
                    <td class="center">
                      <input
                        v-model="device.is_trigger"
                        type="checkbox"
                        @change="setRole(device, 'is_trigger')"
                      />
                    </td>
                    <td class="center">
                      <input
                        v-model="device.follow"
                        type="checkbox"
                        @change="setRole(device, 'follow')"
                      />
                    </td>
                    <td>
                      <select
                        :value="device.leave_to === null ? '' : String(device.leave_to)"
                        @change="setLeaveTo(device, ($event.target as HTMLSelectElement).value)"
                      >
                        <option value="">{{ t("common.none") }}</option>
                        <option
                          v-for="host in cfg.hosts"
                          :key="host.index"
                          :value="String(host.index)"
                        >
                          {{ hostLabel(host) }}
                        </option>
                      </select>
                    </td>
                    <td class="center">
                      <button class="mini" :title="t('common.delete')" @click="removeDevice(i)">
                        −
                      </button>
                    </td>
                  </tr>
                  <tr class="meta">
                    <td colspan="5">
                      <input
                        v-model="device.id"
                        type="text"
                        class="id"
                        :title="t('devices.idTitle')"
                      />
                      <span class="muted">{{ roleLabel(device.role) }}</span>
                      <span class="muted">{{ serialLabel(device) }}</span>
                      <select v-model="device.transport" class="transport" :title="t('devices.transport')">
                        <option v-for="transport in TRANSPORTS" :key="transport" :value="transport">
                          {{ transport }}
                        </option>
                      </select>
                    </td>
                  </tr>
                </template>
              </tbody>
            </table>
          </div>
          <button class="link" @click="addDevice()">{{ t("devices.add") }}</button>
          <button
            class="link"
            :disabled="!granted || scanning"
            :title="granted ? undefined : t('devices.scanNeedsPermission')"
            @click="toggleScan"
          >
            {{ found ? t("scan.collapse") : scanning ? t("scan.scanning") : t("devices.scan") }}
          </button>
          <div v-if="found" class="found">
            <p v-if="found.length === 0" class="muted empty">{{ t("devices.noneFound") }}</p>
            <div
              v-for="(device, i) in found"
              :key="i"
              class="found-row"
              :class="{ dim: alreadyAdded(device) }"
            >
              <span class="found-name">{{ device.name }}</span>
              <code>{{ device.id }}</code>
              <code>{{ device.serial ?? t("devices.noSerial") }}</code>
              <span>
                {{
                  t("devices.currentHost", {
                    channel: device.current_host + 1,
                    count: device.host_count,
                  })
                }}
              </span>
              <span>{{ roleLabel(device.role_guess) }}</span>
              <button class="mini" :disabled="alreadyAdded(device)" @click="addFromScan(device)">
                {{ t("common.add") }}
              </button>
            </div>
          </div>
        </section>
      </template>

      <template v-if="tab === 'advanced' && cfg">
        <section class="card">
          <h2>{{ t("timing.title") }}</h2>
          <div class="row">
            <span>
              {{ t("timing.debounce") }}<span class="muted"> · {{ t("timing.debounceHint") }}</span>
            </span>
            <span class="v">
              <input v-model.number="cfg.timing.debounce_ms" type="number" min="100" class="number" />
              <span class="unit">{{ t("common.ms") }}</span>
            </span>
          </div>
          <div class="row">
            <span>
              {{ t("timing.cooldown") }}<span class="muted"> · {{ t("timing.cooldownHint") }}</span>
            </span>
            <span class="v">
              <input
                v-model.number="cfg.timing.cooldown_ms"
                type="number"
                min="1000"
                class="number"
              />
              <span class="unit">{{ t("common.ms") }}</span>
            </span>
          </div>
          <div class="row">
            <span>{{ t("timing.retries") }}</span>
            <span class="v">
              <input
                v-model.number="cfg.timing.ddc_retries"
                type="number"
                min="0"
                max="255"
                class="number"
              />
              <!-- No unit of its own: the empty one keeps this field's right
                   edge in line with the two above. -->
              <span class="unit"></span>
            </span>
          </div>
        </section>

        <section class="card">
          <h2>{{ t("options.title") }}</h2>
          <label class="row">
            <span>{{ t("options.launchAtLogin") }}</span>
            <input v-model="cfg.options.launch_at_login" type="checkbox" />
          </label>
          <label class="row">
            <span>{{ t("options.switchBack") }}</span>
            <input v-model="cfg.options.switch_back_on_reconnect" type="checkbox" />
          </label>
          <label class="row">
            <span>{{ t("options.pullOnArrival") }}</span>
            <input v-model="cfg.options.pull_on_arrival" type="checkbox" />
          </label>
        </section>

        <section class="card">
          <h2>{{ t("options.languageTitle") }}</h2>
          <label class="row">
            <span>{{ t("options.language") }}</span>
            <span class="v">
              <select v-model="cfg.options.language" class="language">
                <option value="auto">{{ t("options.languageAuto") }}</option>
                <option value="zh-Hans">{{ t("options.languageZhHans") }}</option>
                <option value="en">{{ t("options.languageEn") }}</option>
              </select>
            </span>
          </label>
        </section>
      </template>
    </main>

    <footer>
      <span v-if="saveError" class="note bad" :title="saveError">{{ saveError }}</span>
      <span v-else-if="toast" class="note muted">{{ toast }}</span>
      <span v-else class="note muted">{{ t("footer.savedNote") }}</span>
      <button class="primary" :disabled="!cfg || saving" @click="save">{{ t("common.save") }}</button>
    </footer>
  </div>
</template>

<style scoped>
/* Four layers: header, the tab strip inside it, the scrolling panel and a
   footer that never leaves. The window is resizable, so the height follows it. */
.win {
  height: 100vh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

header {
  flex: none;
  padding: 12px 16px 0;
}

.title {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}

.title b {
  font-size: 15px;
  font-weight: 600;
}

.state {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  font-size: 12px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

main {
  flex: 1;
  overflow: auto;
  padding: 12px 16px 8px;
}

.banner {
  margin: 0 0 10px;
  padding: 6px 8px;
  border: 1px solid var(--bad);
  border-radius: 8px;
  background: var(--bad-bg);
  color: var(--bad);
  white-space: pre-wrap;
}

/* The step name, which carries a display or device name and may not fit. */
.what {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.card h2 .bad {
  font-weight: 500;
}

/* The second line under a row: the identity a scan filled in, not a setting. */
tr.meta td {
  border-top: 0;
  padding: 0 6px 6px 0;
  font-size: 11px;
  color: var(--muted);
}

tr.meta td > * {
  margin-right: 8px;
}

tr.meta input,
tr.meta select {
  display: inline-block;
  width: auto;
  font-size: 11px;
  padding: 1px 5px;
  font-family: ui-monospace, "SF Mono", Menlo, monospace;
}

tr.meta .edid {
  width: 300px;
}

tr.meta .id {
  width: 100px;
}

tr.meta .transport {
  font-family: inherit;
}

/* The input code and, in this machine's column, its read button. */
.input-cell {
  display: flex;
  align-items: center;
  gap: 4px;
}

.input-cell input {
  min-width: 0;
}

.number {
  width: 74px;
}

/* The word after a timing field. Every row on the advanced tab carries one,
   empty where there is no unit, so the three fields share a right edge in
   either language — "ms" and 「毫秒」 both fit inside it. */
.unit {
  flex: none;
  min-width: 2.4em;
}

.language {
  width: 150px;
}

.scroll {
  overflow-x: auto;
}

.found {
  margin-top: 8px;
  max-height: 180px;
  overflow-y: auto;
  border: 1px solid var(--border);
  border-radius: 8px;
}

.found .empty {
  margin: 0;
  padding: 6px 8px;
  font-size: 11px;
}

.found-row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  padding: 5px 8px;
  border-bottom: 1px solid var(--border);
  font-size: 11px;
}

.found-row:last-child {
  border-bottom: 0;
}

/* Already in the table: nothing left to add from this row. */
.found-row.dim {
  opacity: 0.45;
}

.found-name {
  font-size: 13px;
  font-weight: 500;
}

.found-row code,
.found-row span:not(.found-name) {
  color: var(--muted);
}

footer {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  padding: 8px 16px;
  border-top: 1px solid var(--border);
  background: var(--bg);
}

/* One line only: the banner has the full text, this is just the reminder that
   sits next to the save button when that is all the user can see. */
.note {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  font-size: 11px;
}
</style>
