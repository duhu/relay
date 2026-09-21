<script setup lang="ts">
// The whole config.json, editable, in four tabs: hosts, displays, devices,
// timing and which host this machine is. Saving hands the file to the backend,
// which validates it before writing; the core picks the new file up through its
// watcher.
import { desktopDir, join } from "@tauri-apps/api/path";
import { getCurrentWindow } from "@tauri-apps/api/window";
// `save` is this view's own save button, so the panel comes in renamed.
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
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

/** Mirrors `SCHEMA_VERSION` in `crates/relay-core/src/config.rs`. */
const SCHEMA_VERSION = 1;

/**
 * What a Mac with no config file starts from: nothing declared, and everything
 * the form does not ask about at the same defaults the wizard writes on a first
 * run (`timing` from the spec's example config, `options` from the serde
 * defaults of `relay_core::config::Options`).
 *
 * `this_host` is 255, the slot no host can declare — "not chosen yet", the same
 * sentinel the first-run seed carried. It fails validation on purpose: an empty
 * config is not savable, and the backend says what is missing.
 */
function emptyConfig(): Config {
  return {
    schema_version: SCHEMA_VERSION,
    this_host: 255,
    hosts: [],
    displays: [],
    devices: [],
    timing: { debounce_ms: 800, cooldown_ms: 5000, ddc_retries: 3 },
    hotkeys: {},
    options: {
      switch_back_on_reconnect: true,
      pull_on_arrival: true,
      launch_at_login: true,
      language: "auto",
    },
  };
}

const cfg = ref<Config | null>(null);
/** `cfg` is a blank config this window invented, because there is no file. */
const blank = ref(false);
/**
 * `cfg` as it was when the file was last read or written, serialized.
 *
 * This is how `onShown` tells an untouched window from an edited one, which is
 * the only thing it needs to know to decide whether re-reading the file would
 * be a refresh or a theft.
 */
const baseline = ref("");
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
// The input sources each display reported, keyed by its EDID UUID in lower
// case ("" for the first external display). A row with no entry — or an empty
// one — falls back to the number box it always had.
const inputSources = ref<Record<string, ipc.InputSource[]>>({});
// Cells the user switched to typing a code by hand, keyed by the display's
// EDID UUID in lower case and the host: `${uuid}:${host}`.
const customCells = ref<Set<string>>(new Set());
const loadingSources = ref(false);
// The code the first display is showing, for the "current" marker. `readScreen`
// already asks; this keeps the raw answer, which it used to throw away.
const screenInput = ref<number | null>(null);
// Which display `screenInput` came from. Row indices shift when a row is
// deleted; the UUID does not.
const screenUuid = ref<string | null>(null);
// A `readScreen()` that was dropped because the capabilities read had the
// cable. `loadInputSources()` re-fires it once it lets go.
const screenPending = ref(false);
// The same for a `loadInputSources()` dropped because another one was already
// running, and whether that dropped call had asked for a refresh.
const sourcesPending = ref(false);
const sourcesPendingRefresh = ref(false);
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
let unlistenShown: (() => void) | null = null;

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

onMounted(() => {
  void load();
  // The close button hides this window instead of destroying it, so the view
  // outlives being put away and `windows.rs` sends "shown" when it comes back.
  // The listener is registered on this window, which is what the targeted emit
  // on the Rust side reaches.
  void getCurrentWindow()
    .listen("shown", () => void onShown())
    .then((off) => {
      unlistenShown = off;
    })
    .catch((err) => {
      // Without this listener the window shows whatever it happened to be
      // holding when it was put away, for as long as it lives — and it has no
      // other channel to say so.
      console.error("cannot listen for the window being shown again", err);
    });
});

onUnmounted(() => {
  window.clearTimeout(toastTimer);
  for (const timer of saveTimers) window.clearTimeout(timer);
  unlistenShown?.();
});

/**
 * Re-reads what the world outside this window may have changed while it was
 * hidden — and drops what this window itself has no business still showing.
 *
 * The input sources are deliberately not re-read: a capabilities read costs
 * about a second per display, which would be paid on every single reopen, and
 * a list gone stale — the user swapped or woke a monitor while the window was
 * away — costs a row nothing worse than the number box it falls back to when
 * it has no list at all. Re-asking therefore stays where it already is, behind
 * 「扫描显示器」.
 */
async function onShown() {
  // Nothing loaded means nothing to protect: reopening is the retry it used
  // to be when closing the window destroyed it.
  if (!cfg.value) {
    await load();
    return;
  }
  // Both are about something that happened before the window was put away —
  // a read that failed last week should not be the first thing on screen.
  banner.value = "";
  saveError.value = "";
  // Collapsing the scan lists is exactly what destroying the window used to
  // do, and a week-old list offers to add a receiver that may not be paired
  // any more. Rescanning is one click.
  found.value = null;
  foundDisplays.value = null;
  // `cfg` is a snapshot that can now live for days, and the file is the single
  // source of truth (invariant 7 in `docs/overview.md`) — a hand edit
  // underneath it would be silently reverted by the next save. The comparison
  // decides which mistake to avoid: a window nobody has typed into has no
  // reason to keep showing a stale file, and one that has been typed into has
  // every reason not to lose the typing.
  if (JSON.stringify(cfg.value) === baseline.value) await readConfig();
  await refreshStatus();
  try {
    granted.value = await ipc.inputMonitoringGranted();
  } catch {
    /* the permission answer stays as it was; `load()` reports the failure */
  }
  await refreshHere();
  await readScreen();
}

/**
 * Puts the config file into `cfg` — or, when there is no file at all, a blank
 * config, with the note that says so.
 *
 * The two failures `get_config` can report mean opposite things here. No file
 * is the normal state of a Mac nobody has configured yet: the wizard's way out
 * leads straight to this form, and it has to be a form, not an error. A file
 * that is there and unreadable stays an error — offering a blank form over it
 * would invite a save that destroys whatever is in it.
 */
async function readConfig() {
  try {
    cfg.value = await ipc.getConfig();
    blank.value = false;
    baseline.value = JSON.stringify(cfg.value);
    banner.value = "";
  } catch (err) {
    const failure = ipc.asConfigLoadError(err);
    if (failure?.not_found) {
      cfg.value = emptyConfig();
      blank.value = true;
      baseline.value = JSON.stringify(cfg.value);
      banner.value = "";
      return;
    }
    await showBanner(t("error.loadConfig", { detail: failure?.message ?? String(err) }));
  }
}

async function load() {
  // A reload replaces the device and display rows, so the scan lists — whose
  // "already added" state is read off those rows — must not outlive them.
  found.value = null;
  foundDisplays.value = null;
  customCells.value = new Set();
  await readConfig();
  await refreshStatus();
  try {
    granted.value = await ipc.inputMonitoringGranted();
  } catch (err) {
    // A config error, when there is one, is the more useful of the two.
    if (!banner.value) await showBanner(t("error.loadPermission", { detail: String(err) }));
  }
  await refreshHere();
  await readScreen();
  await loadInputSources();
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
    screenInput.value = null;
    screenUuid.value = null;
    return;
  }
  // The capabilities read holds the cable for seconds at a time, long enough
  // for a tab change or a finished switch to be dropped here and leave the
  // header claiming the screen is where it no longer is — so that one is
  // remembered and re-fired rather than lost.
  if (loadingSources.value) {
    screenPending.value = true;
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
    screenInput.value = code;
    screenUuid.value = display.edid_uuid;
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

/**
 * Asks every display this window has not asked yet for its input sources and
 * caches the answers.
 *
 * Slow — seconds, not milliseconds — so it runs when the window opens, when a
 * row is added and when the user rescans, never on a timer. Displays already in
 * the cache are skipped, and so are rows that share a UUID: two hand-added rows
 * both carry "" and would otherwise cost the same monitor two full timeouts.
 *
 * `refresh` asks every configured display again and replaces what the cache
 * holds. Only the scan button passes it: a monitor that stayed silent the first
 * time — asleep, or on another Mac's input — would otherwise keep its empty
 * list for the rest of the window's life, and the scan button is the one manual
 * retry the feature has. Every other caller wants the cheap cached pass.
 *
 * It shares the DDC cable with the two reads above and with the core's own
 * switching, so it waits its turn under exactly the same rules — and, holding
 * the cable longest of anything here, it asks the core for a fresh state first
 * and hands the cable back to the screen read it displaced.
 */
async function loadInputSources(refresh = false) {
  const c = cfg.value;
  if (!c || c.displays.length === 0) return;
  // A pass already has the cable, and it holds it for seconds — long enough for
  // a row added in the meantime to be dropped here and keep the number box for
  // the rest of this window's life. Remembered and re-fired below, the way
  // `readScreen` is.
  if (loadingSources.value) {
    sourcesPending.value = true;
    if (refresh) sourcesPendingRefresh.value = true;
    return;
  }
  if (readingScreen.value || readingInput.value !== null) return;

  loadingSources.value = true;
  try {
    // Nothing polls the status in this window, so the state it holds may be
    // minutes old — and this is the longest the app ever occupies the cable.
    await refreshStatus();
    const state = status.value?.state;
    if (state === "Switching" || state === "Confirming") return;

    const byUuid: Record<string, ipc.InputSource[]> = { ...inputSources.value };
    // What this pass has already asked, so two rows sharing a UUID still cost
    // one round trip even when the cache is being replaced rather than filled.
    const asked = new Set<string>();
    for (const display of c.displays) {
      const key = display.edid_uuid.toLowerCase();
      if (asked.has(key) || (!refresh && key in byUuid)) continue;
      asked.add(key);
      // A display that will not answer costs one round trip and yields an
      // empty list; the row simply keeps its number box.
      byUuid[key] = await ipc.listInputSources(display.edid_uuid);
    }
    inputSources.value = byUuid;
  } catch {
    // Nothing to tell the user: a capabilities read that fails means the
    // number box, which is what is already on screen.
  } finally {
    loadingSources.value = false;
    // A pass that arrived while this one held the cable, re-run now. The flag
    // is cleared before the re-run, so only a real caller can set it again —
    // this cannot chase its own tail — and the re-run skips every display
    // already cached, so it costs a round trip only for the rows that need one.
    // A dropped refresh has to come back as a refresh: the scan button is the
    // only way to re-ask a display that stayed silent.
    if (sourcesPending.value) {
      const again = sourcesPendingRefresh.value;
      sourcesPending.value = false;
      sourcesPendingRefresh.value = false;
      void loadInputSources(again);
    }
    // A screen read that came in while the cable was busy was dropped rather
    // than queued, and the header would go on showing the older answer. It goes
    // after the re-run above, which takes the cable straight back and hands the
    // read on again when it is done.
    if (screenPending.value) {
      screenPending.value = false;
      void readScreen();
    }
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
  // The two shapes a blank form is in before anything has been filled in. The
  // backend refuses both, but in its own words ("this_host 255 is not declared
  // in hosts"), which is the wrong sentence to meet on a Mac's very first save.
  if (c.hosts.length === 0) return t("validate.noHosts");
  for (const [i, host] of c.hosts.entries()) {
    if (!inRange(host.index, 0, 2)) return t("validate.hostChannel", { row: i + 1 });
  }
  if (!c.hosts.some((host) => host.index === c.this_host)) return t("validate.noThisHost");
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
    // What was just written is what the file now holds, so this is the new
    // "untouched" for `onShown` to compare against — and there is now a file,
    // so the note about there not being one goes.
    baseline.value = JSON.stringify(c);
    blank.value = false;
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

/**
 * Writes the config file this Mac is running to a file the user picks, for the
 * next Mac to read. The backend copies the file itself, so what travels is what
 * is running rather than whatever this window currently holds.
 *
 * Closing the panel without choosing a path is an answer, not a failure: it
 * returns `null` and nothing happens.
 */
async function exportConfig() {
  banner.value = "";
  try {
    const path = await saveDialog({
      defaultPath: await join(await desktopDir(), "relay-config.json"),
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (path === null) return;
    await ipc.exportConfig(path);
    showToast(t("toast.exported"));
  } catch (err) {
    await showBanner(t("error.export", { detail: String(err) }));
  }
}

/**
 * Hands the window over to the wizard, which owns both of the questions an
 * import has to ask ("which of these Macs are you", "where does a switch away
 * from here go") — asking them here as well would be the same form twice.
 *
 * `?import` puts the wizard straight on its import screen: the user who
 * pressed「导入配置」has already answered the fork it opens with.
 */
function openWizard(at?: "import") {
  window.location.hash = at ? `#/wizard?${at}` : "#/wizard";
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
  // The new row has no cached source list, so it would show the number box for
  // the rest of this window's life; asking costs nothing for the rows already
  // answered.
  void loadInputSources();
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
    // The manual retry: a display that would not answer earlier gets asked
    // again rather than keeping the empty list it was cached with.
    void loadInputSources(true);
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
  // Scan, add, then set the input is the first-run path, and the row added here
  // is exactly the one the scan's own refresh could not have seen. That refresh
  // is usually still running when this lands — a second or so per display — so
  // this call is normally the one that gets dropped and re-fired from the
  // refresh's `finally`, rather than one that reaches the monitor itself.
  void loadInputSources();
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
  if (readingInput.value !== null || readingScreen.value || loadingSources.value) return;
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
    // The button just asked the same monitor `readScreen` asks, so its answer
    // is the fresher one — otherwise the picker beside it would go on calling
    // the old code "current".
    screenInput.value = code;
    screenUuid.value = uuid;
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

/** The picker's options for one row, or an empty list to keep the number box. */
function sourcesFor(display: DisplayConfig): ipc.InputSource[] {
  return inputSources.value[display.edid_uuid.toLowerCase()] ?? [];
}

/**
 * A cell's identity. By the display, not by its position: deleting a row above
 * it would otherwise re-point every flag onto the wrong cell, mid-edit.
 */
function cellKey(display: DisplayConfig, host: number): string {
  return `${display.edid_uuid.toLowerCase()}:${host}`;
}

/**
 * Whether this cell shows the picker.
 *
 * Only when the display reported a list and the user has not asked to type a
 * code by hand for this particular cell.
 */
function picks(display: DisplayConfig, host: number): boolean {
  return sourcesFor(display).length > 0 && !customCells.value.has(cellKey(display, host));
}

/**
 * The options one cell offers: what the display reported, plus the configured
 * code when the display did not list it (otherwise the cell could not show its
 * own value), plus the escape hatch.
 */
function optionsFor(display: DisplayConfig, host: number): ipc.InputSource[] {
  const sources = sourcesFor(display);
  const current = display.input_by_host[String(host)];
  if (current === undefined || sources.some((source) => source.code === current)) {
    return sources;
  }
  return [...sources, { code: current, name: null }];
}

/**
 * One option's label: the MCCS name, or the bare code for an input the
 * standard does not name. No "current" marker — a closed `<select>` shows
 * this text in a fixed-width box, and the marker is now the optgroup the
 * option sits in.
 */
function optionLabel(source: ipc.InputSource): string {
  return source.name ?? t("displays.inputCode", { code: source.code });
}

/**
 * The option the monitor says it is showing, when this row is the one
 * `readScreen` (or the row's own read button) actually asked. A row nobody
 * read gets `null`: it must not claim to know.
 */
function currentOption(display: DisplayConfig, host: number): ipc.InputSource | null {
  if (screenInput.value === null || screenUuid.value === null) return null;
  if (screenUuid.value.toLowerCase() !== display.edid_uuid.toLowerCase()) return null;
  return optionsFor(display, host).find((s) => s.code === screenInput.value) ?? null;
}

/** Everything `currentOption` did not take, in the order the monitor gave. */
function otherOptions(display: DisplayConfig, host: number): ipc.InputSource[] {
  const current = currentOption(display, host);
  const all = optionsFor(display, host);
  return current === null ? all : all.filter((s) => s.code !== current.code);
}

function choose(display: DisplayConfig, host: number, raw: string) {
  if (raw === "custom") {
    customCells.value = new Set(customCells.value).add(cellKey(display, host));
    return;
  }
  if (raw === "") {
    delete display.input_by_host[String(host)];
    return;
  }
  display.input_by_host[String(host)] = Number(raw);
}

/**
 * Leaves the hand-typed code behind and goes back to the monitor's own
 * list. The value stays put — `optionsFor` keeps a code the monitor did
 * not list as an option of its own, so nothing has to be re-picked.
 */
function backToList(display: DisplayConfig, host: number) {
  const next = new Set(customCells.value);
  next.delete(cellKey(display, host));
  customCells.value = next;
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
      <!--
        The way out of a config file that does not parse. Everything else in
        this window is behind `v-if="cfg"`, which such a file never fills, so
        without this button the only way back to the wizard is to quit the app
        and start it again. It sits outside that guard, and only appears when
        there is no config: a banner over a form the user can still edit is not
        a dead end.
      -->
      <p v-if="banner" ref="bannerEl" class="banner">
        <span>{{ banner }}</span>
        <button v-if="!cfg" class="mini" @click="openWizard()">
          {{ t("error.loadConfigWizard") }}
        </button>
      </p>
      <p v-if="blank" class="blank">{{ t("blank.note") }}</p>

      <template v-if="tab === 'overview'">
        <section class="card">
          <h2>{{ t("overview.thisMac") }}</h2>
          <p class="hero">
            <b>{{ thisHost ? hostName(thisHost) : t("common.unknown") }}</b>
            <span class="muted">{{ heroDetail }}</span>
            <span class="pill" :class="toneClass">{{ stateLabel }}</span>
          </p>
          <!--
            A Mac with no config file has already been told so, gently, by the
            note above the form. The core's own words for the same fact are the
            operating system's ("No such file or directory"), and in red under
            that note they read as a fault on a machine where nothing is wrong
            yet. A file that exists and does not load still says so here.
          -->
          <div v-if="status && !status.config_ok && !blank" class="row">
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
                    style="width: 136px"
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
                        <select
                          v-if="picks(display, host.index)"
                          :value="inputFor(display, host.index)"
                          @change="
                            choose(display, host.index, ($event.target as HTMLSelectElement).value)
                          "
                        >
                          <option value="">{{ t("displays.inputEmpty") }}</option>
                          <template v-if="currentOption(display, host.index)">
                            <optgroup :label="t('displays.inputGroupCurrent')">
                              <option
                                :key="currentOption(display, host.index)!.code"
                                :value="currentOption(display, host.index)!.code"
                              >
                                {{ optionLabel(currentOption(display, host.index)!) }}
                              </option>
                            </optgroup>
                            <optgroup :label="t('displays.inputGroupOther')">
                              <option
                                v-for="source in otherOptions(display, host.index)"
                                :key="source.code"
                                :value="source.code"
                              >
                                {{ optionLabel(source) }}
                              </option>
                            </optgroup>
                          </template>
                          <template v-else>
                            <option
                              v-for="source in optionsFor(display, host.index)"
                              :key="source.code"
                              :value="source.code"
                            >
                              {{ optionLabel(source) }}
                            </option>
                          </template>
                          <option value="custom">{{ t("displays.inputCustom") }}</option>
                        </select>
                        <template v-else>
                          <input
                            type="number"
                            min="0"
                            max="255"
                            :value="inputFor(display, host.index)"
                            @input="
                              setInput(
                                display,
                                host.index,
                                ($event.target as HTMLInputElement).value,
                              )
                            "
                          />
                          <button
                            v-if="sourcesFor(display).length > 0"
                            class="mini"
                            :title="t('displays.inputBackToList')"
                            @click="backToList(display, host.index)"
                          >
                            ↩
                          </button>
                        </template>
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
                      <div class="input-cell">
                        <span class="muted">{{ t("displays.edid") }}</span>
                        <input
                          v-model="display.edid_uuid"
                          type="text"
                          class="edid"
                          :title="t('displays.edidHint')"
                        />
                        <button
                          v-if="cfg.hosts.some((host) => host.index === cfg!.this_host)"
                          class="mini"
                          :title="loadingSources ? t('displays.readBusy') : t('displays.readTitle')"
                          :disabled="readingInput !== null || readingScreen || loadingSources"
                          @click="readInput(display, cfg.this_host, i)"
                        >
                          {{ readingInput === i ? t("displays.reading") : t("displays.readLong") }}
                        </button>
                      </div>
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

        <section class="card">
          <h2>{{ t("transfer.title") }}</h2>
          <div class="row">
            <span class="muted">{{ t("transfer.hint") }}</span>
            <span class="v">
              <button class="mini" @click="exportConfig">{{ t("transfer.export") }}</button>
              <button class="mini" @click="openWizard('import')">{{ t("transfer.import") }}</button>
              <button class="mini" @click="openWizard()">{{ t("transfer.rerun") }}</button>
            </span>
          </div>
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

/* The banner's own button, when it carries one: clear of the message, and
   free to wrap below it when the message takes the whole width. */
.banner .mini {
  margin-left: 8px;
}

/* Not a failure, so not the banner's red: a blank form is a normal first run. */
.blank {
  margin: 0 0 10px;
  padding: 6px 8px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel);
  color: var(--muted);
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

/* The EDID line is also where this machine's read button lives, so it is a
   flex row. The label and the button keep their own width and the uuid field
   takes what is left — it must not collapse to nothing behind a long label. */
tr.meta .input-cell > .muted,
tr.meta .input-cell > button {
  flex: none;
}

tr.meta .input-cell > .edid {
  flex: 1 1 300px;
  width: auto;
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

/* The picker stands in for the number box, so it has to shrink the same way
   inside the flex row. Height comes from the shared control rule in style.css. */
.input-cell select {
  width: 100%;
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
