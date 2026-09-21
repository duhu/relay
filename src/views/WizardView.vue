<script setup lang="ts">
// The first thing a Mac that has never been configured sees, and the fast path
// for a second one. Two routes out of one fork: build a config here, or adopt
// the one another Mac exported.
//
// Nothing reaches the disk until the last screen. Every answer lives in a ref
// in this component, and `saveConfig` / `importConfig` is called exactly once,
// so going back a step — or closing the window, which only hides it — leaves
// the machine exactly as the wizard found it.
import { desktopDir, join } from "@tauri-apps/api/path";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { computed, onMounted, ref, watch, watchEffect } from "vue";

import { t } from "../lib/i18n";
import * as ipc from "../lib/ipc";
import type {
  Config,
  DiscoveredDevice,
  DiscoveredDisplay,
  InputSource,
  Options,
} from "../lib/ipc";

const props = defineProps<{
  /** Skip the fork and land straight on the import screen. */
  startAtImport?: boolean;
  /**
   * Whether the wizard is the only thing this window can show — a machine that
   * is not configured — rather than something the user opened from Advanced.
   * It changes what the way out is called, because there is nothing to cancel
   * back to on a machine that has no settings to return to.
   */
  onlyView?: boolean;
}>();

// The wizard has nothing to hand back: what it built is on disk, and the
// settings view reads the file itself. `exit` is the other way out — the user
// leaving the wizard without it having written anything.
const emit = defineEmits<{ (event: "done"): void; (event: "exit"): void }>();

type Screen =
  | "choice"
  | "importFile"
  | "importLeave"
  | "permission"
  | "devices"
  | "machines"
  | "displays"
  | "done";

/**
 * The numbered screens of the first-Mac path. The import path is one or two
 * questions long, which is shorter than the counter is worth.
 */
const FIRST_RUN: Screen[] = ["permission", "devices", "machines", "displays"];

const TITLE_KEYS: Record<Screen, string> = {
  choice: "wizard.choiceTitle",
  importFile: "wizard.importTitle",
  importLeave: "wizard.importTitle",
  permission: "wizard.permissionTitle",
  devices: "wizard.devicesTitle",
  machines: "wizard.machinesTitle",
  displays: "wizard.displaysTitle",
  done: "wizard.doneTitle",
};

/** Where each screen's back button goes; a screen without one is not listed. */
const BACK_TO: Partial<Record<Screen, Screen>> = {
  importFile: "choice",
  importLeave: "importFile",
  permission: "choice",
  devices: "permission",
  machines: "devices",
  displays: "machines",
};

/** Mirrors the timing in the spec's example config; the wizard never asks. */
const DEFAULT_TIMING = { debounce_ms: 800, cooldown_ms: 5000, ddc_retries: 3 };

/** The serde defaults of `relay_core::config::Options`; the wizard never asks. */
const DEFAULT_OPTIONS: Options = {
  switch_back_on_reconnect: true,
  pull_on_arrival: true,
  launch_at_login: true,
  language: "auto",
};

/** Mirrors `SCHEMA_VERSION` in `crates/relay-core/src/config.rs`. */
const SCHEMA_VERSION = 1;

/** One row of the machines table. `index` is the channel, 0-based. */
interface HostRow {
  index: number;
  name: string;
}

const screen = ref<Screen>(props.startAtImport ? "importFile" : "choice");
/** The one red line at the top: whatever the last call refused to do. */
const error = ref("");
/** A call the footer has to wait for — a scan, a save, an import. */
const busy = ref(false);

// --- the import path -------------------------------------------------------

const filePath = ref("");
const fileCfg = ref<Config | null>(null);
/** Which host in the file is this Mac; a channel, not a row. */
const fileHost = ref<number | null>(null);
const fileLeaveTo = ref<number | null>(null);
/**
 * The channel the keyboard says this Mac occupies, when one answered. It is
 * the default answer to "which of these are you", and the sentence beside it.
 */
const keyboardChannel = ref<number | null>(null);

// --- the first-Mac path ----------------------------------------------------

const granted = ref(false);
/** The scan result; `null` until the first scan has run. */
const found = ref<DiscoveredDevice[] | null>(null);
const scanning = ref(false);
const triggerId = ref("");
const followId = ref("");
/** This Mac's channel when the two chosen devices disagree and the user picked. */
const channelPick = ref<number | null>(null);
const hostRows = ref<HostRow[]>([]);
/** Which row of the table is this Mac. A position, so renumbering cannot break it. */
const thisRow = ref(0);
/** Which row a trigger leaves to; only asked, and only used, with three Macs. */
const leaveRow = ref<number | null>(null);

const displays = ref<DiscoveredDisplay[] | null>(null);
const scanningDisplays = ref(false);
/** The chosen display's EDID UUID, or `null` for "no shared display". */
const chosenUuid = ref<string | null>(null);
/** The input code per host, keyed like `DisplayConfig.input_by_host`. */
const inputs = ref<Record<string, number | "">>({});
const sources = ref<InputSource[]>([]);
const loadingSources = ref(false);
/** Hosts whose cell the user switched from the picker to a typed code. */
const typed = ref<Set<number>>(new Set());
const readingInput = ref(false);

// --- what the wizard never asks about --------------------------------------

// `timing`, `hotkeys` and `options` are the wizard's blind spot: no screen here
// asks about them, so it must not write over them. On a first run they are the
// defaults; on a rerun from Advanced they are whatever the live config holds,
// or a tuned debounce, a set of global hotkeys and an explicit language would
// quietly go back to the defaults.
const timing = ref({ ...DEFAULT_TIMING });
const hotkeys = ref<Record<string, string>>({});
const options = ref<Options>({ ...DEFAULT_OPTIONS });

/** Keeps the three above from whatever is on disk, when anything is. */
async function keepUnasked() {
  try {
    const cfg = await ipc.getConfig();
    timing.value = { ...cfg.timing };
    hotkeys.value = { ...cfg.hotkeys };
    options.value = { ...cfg.options };
  } catch {
    /* nothing on disk to keep: this is a first run, and the defaults stand */
  }
}

// --- the last screen -------------------------------------------------------

/** Whether the done screen offers to export — only the path that built a file. */
const offerExport = ref(false);
const exported = ref(false);

// The native title bar is outside the Vue tree, so the window title has to be
// set from here; `t()` reads the language ref, so this re-runs when the
// language changes. `SettingsView` sets its own title once the wizard is done.
watchEffect(() => {
  const title = t("wizard.title");
  getCurrentWindow()
    .setTitle(title)
    .catch(() => {
      /* a title the window manager refused is not worth a banner */
    });
});

onMounted(() => {
  void keepUnasked();
  // The import screen can be the very first one, via `#/wizard?import`.
  if (screen.value === "importFile") void enterImportFile();
});

/**
 * The way out of the wizard, on every screen. With nothing to go back to it
 * says where it leads instead — the settings form, filled in by hand.
 */
const exitLabel = computed(() => (props.onlyView ? t("wizard.byHand") : t("wizard.cancel")));

/** The step counter, or `null` on a screen that is not one of the four. */
const stepNumber = computed(() => {
  const at = FIRST_RUN.indexOf(screen.value);
  return at === -1 ? null : at + 1;
});

const canBack = computed(() => BACK_TO[screen.value] !== undefined);

/**
 * Moves to `next`, clearing the banner first: every message this view shows
 * belongs to the screen that produced it.
 */
function go(next: Screen) {
  error.value = "";
  screen.value = next;
}

function back() {
  const target = BACK_TO[screen.value];
  if (target) go(target);
}

// --- the fork --------------------------------------------------------------

function chooseFirstMac() {
  go("permission");
  void refreshPermission();
}

function chooseImport() {
  go("importFile");
  void enterImportFile();
}

// --- the import path -------------------------------------------------------

/**
 * Asks the keyboard where it thinks it is, once, so the "which of these are
 * you" question has a default. A scan that fails or finds nothing is not worth
 * a banner here: the user still answers, just without the hint.
 */
async function enterImportFile() {
  if (keyboardChannel.value !== null) return;
  try {
    const devices = await ipc.scanDevices();
    const channels = new Set(devices.map((device) => device.current_host));
    // Two devices on different channels cannot both be this Mac, and guessing
    // which one is right would be worse than not guessing at all.
    if (channels.size === 1) keyboardChannel.value = [...channels][0];
  } catch {
    /* no hint, no harm: the file's machine list is still on screen */
  }
}

/**
 * Reads a config another Mac exported, without touching this Mac's own file.
 *
 * Closing the panel without choosing is an answer, not a failure: it returns
 * `null` and nothing happens.
 */
async function pickFile() {
  error.value = "";
  try {
    const picked = await openDialog({
      multiple: false,
      directory: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (picked === null || Array.isArray(picked)) return;
    busy.value = true;
    const cfg = await ipc.readConfigFile(picked);
    filePath.value = picked;
    fileCfg.value = cfg;
    // The file's own `this_host` describes the Mac that exported it, so the
    // keyboard is the only thing here that knows anything about this one.
    const hinted = cfg.hosts.some((host) => host.index === keyboardChannel.value);
    fileHost.value = hinted ? keyboardChannel.value : null;
    fileLeaveTo.value = null;
  } catch (err) {
    fileCfg.value = null;
    error.value = t("wizard.errorRead", { detail: String(err) });
  } finally {
    busy.value = false;
  }
}

/** With exactly two machines the answer is the other one, so it is not asked. */
const importNeedsLeaveTo = computed(() => (fileCfg.value?.hosts.length ?? 0) > 2);

/** The machines in the file that this Mac could hand its devices to. */
const importLeaveOptions = computed(() =>
  (fileCfg.value?.hosts ?? []).filter((host) => host.index !== fileHost.value),
);

/**
 * Adopts the file. The command ignores the file's own `this_host` and
 * `leave_to` and writes the two answers above in their place.
 */
async function doImport() {
  busy.value = true;
  try {
    await ipc.importConfig(filePath.value, fileHost.value!, fileLeaveTo.value);
    offerExport.value = false;
    go("done");
  } catch (err) {
    error.value = t("wizard.errorImport", { detail: String(err) });
  } finally {
    busy.value = false;
  }
}

// --- step 1: the permission ------------------------------------------------

async function refreshPermission() {
  try {
    granted.value = await ipc.inputMonitoringGranted();
  } catch (err) {
    error.value = t("error.loadPermission", { detail: String(err) });
  }
}

async function requestPermission() {
  error.value = "";
  try {
    granted.value = await ipc.requestInputMonitoring();
  } catch (err) {
    error.value = t("error.requestPermission", { detail: String(err) });
  }
}

async function openPrivacySettings() {
  try {
    await ipc.openPrivacySettings();
  } catch (err) {
    error.value = t("error.openSettings", { detail: String(err) });
  }
}

// --- step 2: the keyboard and the mouse ------------------------------------

/** Scans once on arrival; the rescan link is the way to ask again. */
async function enterDevices() {
  if (found.value === null) await scan();
}

async function scan() {
  scanning.value = true;
  error.value = "";
  try {
    found.value = await ipc.scanDevices();
  } catch (err) {
    // An empty list, not a null one: a scan that failed is a scan that found
    // nothing, and the skip link below has to be reachable either way.
    found.value = [];
    error.value = t("error.scanDevices", { detail: String(err) });
  } finally {
    scanning.value = false;
  }
}

/** Trigger and follow are exclusive per device, exactly as `validate()` says. */
function pickTrigger(device: DiscoveredDevice) {
  triggerId.value = device.id;
  if (followId.value === device.id) followId.value = "";
  channelPick.value = null;
}

function pickFollow(device: DiscoveredDevice) {
  followId.value = device.id;
  if (triggerId.value === device.id) triggerId.value = "";
  channelPick.value = null;
}

const chosenDevices = computed(() =>
  (found.value ?? []).filter(
    (device) => device.id === triggerId.value || device.id === followId.value,
  ),
);

/** The channels the chosen devices report; more than one means they disagree. */
const channelOptions = computed(() => [
  ...new Set(chosenDevices.value.map((device) => device.current_host)),
]);

/**
 * This Mac's channel, as the keyboard and the mouse see it. `null` while they
 * disagree and the user has not said which of them to believe.
 */
const deviceChannel = computed(() => {
  if (channelOptions.value.length === 1) return channelOptions.value[0];
  return channelPick.value;
});

/** The scan found nothing to pick, so the step may be skipped past. */
const noDevices = computed(() => found.value !== null && found.value.length === 0);

// --- step 3: the machines --------------------------------------------------

/**
 * Fills the table in from what the keyboard reported, the first time only:
 * coming back from the display step must not throw away a typed name.
 *
 * The mark on "This Mac" is the exception. Going back and choosing another
 * keyboard, or another channel for the pair, changes which channel this Mac is
 * on, and the table would otherwise keep pointing at the old row while the
 * previous screen says something else.
 */
function enterMachines() {
  const channel = deviceChannel.value ?? 0;
  if (hostRows.value.length > 0) {
    const at = hostRows.value.findIndex((row) => row.index === channel);
    if (at !== -1) thisRow.value = at;
    return;
  }
  const reported = Math.max(...chosenDevices.value.map((device) => device.host_count), 2);
  // Three is as far as HID++ Easy-Switch goes, and a fourth row would only
  // repeat the third. A channel beyond the reported count still needs a row.
  const count = Math.min(Math.max(reported, channel + 1), 3);
  hostRows.value = Array.from({ length: count }, (_, i) => ({
    index: i,
    name: t("hosts.newName", { channel: i + 1 }),
  }));
  thisRow.value = Math.min(channel, count - 1);
  leaveRow.value = null;
}

/**
 * The channel column edits `index + 1`, like every other channel label. A value
 * outside 1–3 is simply not taken: the field re-syncs on blur, and the finish
 * check has the last word on duplicates.
 */
function setRowChannel(row: HostRow, raw: string) {
  const value = Number(raw);
  if (raw === "" || !Number.isInteger(value) || value < 1 || value > 3) return;
  row.index = value - 1;
}

/** The lowest channel no row has yet, or `null` when all three are taken. */
function freeChannel(): number | null {
  const used = new Set(hostRows.value.map((row) => row.index));
  return [0, 1, 2].find((index) => !used.has(index)) ?? null;
}

const canAddMachine = computed(() => hostRows.value.length < 3 && freeChannel() !== null);

function addMachine() {
  const free = freeChannel();
  if (free === null) return;
  hostRows.value.push({ index: free, name: t("hosts.newName", { channel: free + 1 }) });
}

function removeMachine(position: number) {
  if (hostRows.value.length <= 2) return;
  hostRows.value.splice(position, 1);
  // `thisRow` points at a row by position, so a row leaving from above it moves
  // it up one; the row that left takes the answer with it.
  if (position < thisRow.value) thisRow.value -= 1;
  else if (position === thisRow.value) thisRow.value = 0;
  // The exit pointed at a position too, and the table it pointed into is gone.
  leaveRow.value = null;
}

/**
 * With three machines the trigger's exit cannot be inferred — `validate()`
 * rejects a config that leaves it out — so the table asks for it. With two it
 * is the other machine, and nothing is asked.
 */
const needsLeaveTo = computed(() => hostRows.value.length > 2);

const leaveOptions = computed(() =>
  hostRows.value.map((row, position) => ({ row, position })).filter((entry) => entry.position !== thisRow.value),
);

// A machine cannot leave to itself, and the row list above drops the exit's
// radio the moment it becomes this Mac — so the answer has to go with it, or
// the footer would still say the step is done and the backend would refuse the
// save on the screen after this one. The import path drops `fileLeaveTo` the
// same way when the user changes which machine they are.
watch(thisRow, (position) => {
  if (leaveRow.value === position) leaveRow.value = null;
});

// --- step 4: the display ---------------------------------------------------

async function enterDisplays() {
  // A channel edited on the previous step leaves a code behind under the old
  // key, which would travel into the config as a host nothing declares.
  const live = new Set(hostRows.value.map((row) => row.index));
  for (const key of Object.keys(inputs.value)) {
    if (!live.has(Number(key))) delete inputs.value[key];
  }
  // `typed` is keyed by channel too, and a cell left in the typed state under a
  // channel no row has any more would decide the layout of a cell that is gone.
  typed.value = new Set([...typed.value].filter((host) => live.has(host)));
  if (displays.value === null) await scanDisplays();
}

async function scanDisplays() {
  scanningDisplays.value = true;
  error.value = "";
  try {
    const list = await ipc.listDisplays();
    displays.value = list;
    // One display is the whole point of the product; asking which one would be
    // a question with a single answer. With two there is a real question, and
    // guessing would also spend a multi-second read on the wrong monitor.
    if (chosenUuid.value === null && list.length === 1) await chooseDisplay(list[0].edid_uuid);
  } catch (err) {
    displays.value = [];
    error.value = t("error.scanDisplays", { detail: String(err) });
  } finally {
    scanningDisplays.value = false;
  }
}

/**
 * Picks a display and asks it the two questions the table needs: what it is
 * showing right now — which is this Mac's own cable, so it is this Mac's code —
 * and what else it can show, for the other machines' pickers.
 */
async function chooseDisplay(uuid: string | null) {
  chosenUuid.value = uuid;
  inputs.value = {};
  sources.value = [];
  typed.value = new Set();
  if (uuid === null) return;
  // Both calls take the display they were started for, and drop what they got
  // if the choice moved on meanwhile: the two answers on this screen have to
  // come from the same monitor.
  await readThisInput(uuid);
  if (chosenUuid.value !== uuid) return;
  await loadSources(uuid);
}

const chosenDisplay = computed(
  () => displays.value?.find((display) => display.edid_uuid === chosenUuid.value) ?? null,
);

/** This Mac's own row: the monitor answers over this Mac's cable, nobody else's. */
async function readThisInput(uuid: string | null = chosenUuid.value) {
  if (uuid === null || readingInput.value || loadingSources.value) return;
  readingInput.value = true;
  try {
    const code = await ipc.readDisplayInput(uuid);
    // A read takes seconds. A display chosen in the meantime owns the table
    // now, and this code belongs to the one the user left.
    if (chosenUuid.value !== uuid) return;
    if (code === null) {
      error.value = t("displays.readUnsupported");
      return;
    }
    const row = hostRows.value[thisRow.value];
    if (row) inputs.value[String(row.index)] = code;
  } catch (err) {
    if (chosenUuid.value !== uuid) return;
    error.value = t("error.readInput", { detail: String(err) });
  } finally {
    readingInput.value = false;
  }
}

/**
 * The names the other machines' cells are picked from. A display that will not
 * answer yields an empty list, and every cell falls back to a number box —
 * which is what the settings window does with the same silence.
 */
async function loadSources(uuid: string | null = chosenUuid.value) {
  if (uuid === null || loadingSources.value) return;
  loadingSources.value = true;
  try {
    const list = await ipc.listInputSources(uuid);
    // As above: the list of a display the user is no longer on would put that
    // monitor's names in front of this one's codes.
    if (chosenUuid.value !== uuid) return;
    sources.value = list;
  } catch {
    if (chosenUuid.value === uuid) sources.value = [];
  } finally {
    loadingSources.value = false;
  }
}

/** A display's answers are on their way; the radios wait for them. */
const displayBusy = computed(() => readingInput.value || loadingSources.value);

/** Whether this host's cell shows the picker rather than the number box. */
function picks(host: number): boolean {
  return sources.value.length > 0 && !typed.value.has(host);
}

/**
 * What one cell offers: what the display reported, plus the code already in the
 * cell when the display did not list it — otherwise the cell could not show its
 * own value.
 */
function optionsFor(host: number): InputSource[] {
  const current = inputs.value[String(host)];
  if (current === "" || current === undefined || sources.value.some((s) => s.code === current)) {
    return sources.value;
  }
  return [...sources.value, { code: current, name: null }];
}

function optionLabel(source: InputSource): string {
  return source.name ?? t("displays.inputCode", { code: source.code });
}

function choose(host: number, raw: string) {
  if (raw === "custom") {
    typed.value = new Set(typed.value).add(host);
    return;
  }
  inputs.value[String(host)] = raw === "" ? "" : Number(raw);
}

function backToList(host: number) {
  const next = new Set(typed.value);
  next.delete(host);
  typed.value = next;
}

function setInput(host: number, raw: string) {
  const value = Number(raw);
  inputs.value[String(host)] = raw === "" || Number.isNaN(value) ? "" : Math.trunc(value);
}

// --- finishing -------------------------------------------------------------

/**
 * The config the four steps add up to.
 *
 * `timing`, `hotkeys` and `options` come from `keepUnasked()`, not from this
 * screen: the wizard never asks about them, and the advanced tab does.
 */
function buildConfig(): Config {
  const rows = hostRows.value;
  const here = rows[thisRow.value];
  const exit = leaveRow.value === null ? null : (rows[leaveRow.value]?.index ?? null);
  const display = chosenDisplay.value;
  const byHost: Record<string, number> = {};
  for (const row of rows) {
    const code = inputs.value[String(row.index)];
    if (typeof code === "number") byHost[String(row.index)] = code;
  }
  const devices = (found.value ?? [])
    .filter((device) => device.id === triggerId.value || device.id === followId.value)
    .map((device) => ({
      id: device.id,
      name: device.name,
      role: device.role_guess,
      transport: "ble",
      serial: device.serial,
      is_trigger: device.id === triggerId.value,
      follow: device.id === followId.value,
      // Only a trigger ever leaves this Mac on its own, and only three-machine
      // setups have to say where to.
      leave_to: device.id === triggerId.value && needsLeaveTo.value ? exit : null,
    }));
  return {
    schema_version: SCHEMA_VERSION,
    this_host: here?.index ?? 0,
    hosts: rows.map((row) => ({ index: row.index, name: row.name })),
    displays: display
      ? [{ edid_uuid: display.edid_uuid, name: display.name, input_by_host: byHost }]
      : [],
    devices,
    timing: { ...timing.value },
    hotkeys: { ...hotkeys.value },
    options: { ...options.value },
  };
}

/**
 * The shapes the backend would reject, said in this window's language.
 *
 * `save_config` validates too, and its message is shown word for word when it
 * refuses — but it speaks English, and these four are the mistakes a wizard can
 * actually produce, so they get a sentence the user can act on.
 */
function problem(cfg: Config): string {
  if (!cfg.devices.some((device) => device.is_trigger && !device.follow)) {
    return t("wizard.needDevices");
  }
  if (!cfg.devices.some((device) => device.follow && !device.is_trigger)) {
    return t("wizard.needDevices");
  }
  const channels = new Set(cfg.hosts.map((host) => host.index));
  if (channels.size !== cfg.hosts.length) return t("wizard.duplicateChannel");
  if (needsLeaveTo.value && leaveRow.value === null) return t("wizard.needLeaveTo");
  for (const display of cfg.displays) {
    const codes = new Set<number>();
    for (const host of cfg.hosts) {
      const code = display.input_by_host[String(host.index)];
      if (code === undefined) {
        return t("wizard.needInput", { name: host.name || t("common.unnamed") });
      }
      if (codes.has(code)) return t("wizard.duplicateInput", { code });
      codes.add(code);
    }
  }
  return "";
}

/** The one write of the first-Mac path. A refusal stays on this screen. */
async function finish() {
  const cfg = buildConfig();
  const said = problem(cfg);
  if (said) {
    error.value = said;
    return;
  }
  busy.value = true;
  try {
    await ipc.saveConfig(cfg);
    offerExport.value = true;
    go("done");
  } catch (err) {
    // The backend's `ConfigError` is still English, so it goes in as the detail
    // of a localized heading — the same shape the settings window uses.
    error.value = t("error.save", { detail: String(err) });
  } finally {
    busy.value = false;
  }
}

/**
 * Hands the file just written to the next Mac. The backend copies the live
 * file, so what travels is what this wizard saved a moment ago.
 */
async function exportConfig() {
  error.value = "";
  try {
    const path = await saveDialog({
      defaultPath: await join(await desktopDir(), "relay-config.json"),
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (path === null) return;
    await ipc.exportConfig(path);
    exported.value = true;
  } catch (err) {
    error.value = t("error.export", { detail: String(err) });
  }
}

// --- the footer ------------------------------------------------------------

/** Whether this screen's forward button writes the config rather than advancing. */
const isLastStep = computed(() => {
  if (screen.value === "displays") return true;
  if (screen.value === "importLeave") return true;
  return screen.value === "importFile" && fileCfg.value !== null && !importNeedsLeaveTo.value;
});

const canNext = computed(() => {
  switch (screen.value) {
    case "importFile":
      return fileCfg.value !== null && fileHost.value !== null;
    case "importLeave":
      return fileLeaveTo.value !== null;
    case "permission":
      return granted.value;
    case "devices":
      return noDevices.value || (triggerId.value !== "" && followId.value !== "" && deviceChannel.value !== null);
    case "machines":
      return hostRows.value.length >= 2 && (!needsLeaveTo.value || leaveRow.value !== null);
    case "displays":
      return true;
    default:
      return false;
  }
});

/** The line beside the forward button: why it is off, or what it will do. */
const footNote = computed(() => {
  if (screen.value === "devices" && !canNext.value) {
    return channelOptions.value.length > 1 ? t("wizard.pickChannel") : t("wizard.pickDevices");
  }
  if (screen.value === "machines" && !canNext.value) return t("wizard.needLeaveTo");
  if (screen.value === "permission" && !granted.value) return t("wizard.permissionNeeded");
  if (screen.value === "importFile" && fileCfg.value === null) return t("wizard.importNoFile");
  return "";
});

async function next() {
  switch (screen.value) {
    case "importFile":
      if (importNeedsLeaveTo.value) {
        // Coming back and choosing a different machine can leave the exit
        // pointing at this Mac, which `adopt` rejects outright.
        if (fileLeaveTo.value === fileHost.value) fileLeaveTo.value = null;
        go("importLeave");
      } else {
        await doImport();
      }
      return;
    case "importLeave":
      await doImport();
      return;
    case "permission":
      go("devices");
      await enterDevices();
      return;
    case "devices":
      go("machines");
      enterMachines();
      return;
    case "machines":
      go("displays");
      await enterDisplays();
      return;
    case "displays":
      await finish();
      return;
    default:
  }
}
</script>

<template>
  <div class="win">
    <header>
      <div class="title">
        <b>{{ t(TITLE_KEYS[screen]) }}</b>
        <span class="aside">
          <span v-if="stepNumber" class="muted step">
            {{ t("wizard.stepOf", { step: stepNumber, total: FIRST_RUN.length }) }}
          </span>
          <!-- The way out, on every screen the wizard can be stuck on. The done
               screen has its own, and it is the one that belongs there. -->
          <button v-if="screen !== 'done'" class="link" @click="emit('exit')">
            {{ exitLabel }}
          </button>
        </span>
      </div>
    </header>

    <main>
      <p v-if="error" class="banner">{{ error }}</p>

      <!-- Step 0: which of the two wizards this is. -->
      <section v-if="screen === 'choice'" class="card">
        <p class="sub">{{ t("wizard.choiceHint") }}</p>
        <div class="fork">
          <button class="big" @click="chooseFirstMac">
            <b>{{ t("wizard.choiceFirst") }}</b>
            <span class="muted">{{ t("wizard.choiceFirstHint") }}</span>
          </button>
          <button class="big" @click="chooseImport">
            <b>{{ t("wizard.choiceImport") }}</b>
            <span class="muted">{{ t("wizard.choiceImportHint") }}</span>
          </button>
        </div>
      </section>

      <!-- The import path, screen 1: the file and who this Mac is in it. -->
      <template v-if="screen === 'importFile'">
        <section class="card">
          <p class="sub">{{ t("wizard.importHint") }}</p>
          <div class="row">
            <span class="path">{{ filePath || t("wizard.importNoFile") }}</span>
            <span class="v">
              <button class="mini" :disabled="busy" @click="pickFile">
                {{ fileCfg ? t("wizard.importChange") : t("wizard.importPick") }}
              </button>
            </span>
          </div>
        </section>

        <section v-if="fileCfg" class="card">
          <h2>{{ t("wizard.importWhich") }}</h2>
          <label v-for="host in fileCfg.hosts" :key="host.index" class="row">
            <span>
              <input v-model="fileHost" type="radio" :value="host.index" />
              {{ host.name || t("common.unnamed") }}
              <span class="muted"> · {{ t("overview.channelOnly", { channel: host.index + 1 }) }}</span>
            </span>
            <span v-if="host.index === keyboardChannel" class="v">
              {{ t("wizard.keyboardSays", { channel: host.index + 1 }) }}
            </span>
          </label>
        </section>
      </template>

      <!-- The import path, screen 2: only with three machines or more. -->
      <section v-if="screen === 'importLeave'" class="card">
        <h2>{{ t("wizard.leaveTitle") }}</h2>
        <p class="sub">{{ t("wizard.leaveHint") }}</p>
        <label v-for="host in importLeaveOptions" :key="host.index" class="row">
          <span>
            <input v-model="fileLeaveTo" type="radio" :value="host.index" />
            {{ host.name || t("common.unnamed") }}
            <span class="muted"> · {{ t("overview.channelOnly", { channel: host.index + 1 }) }}</span>
          </span>
        </label>
      </section>

      <!-- Step 1: Input Monitoring. -->
      <section v-if="screen === 'permission'" class="card">
        <p class="sub">{{ t("wizard.permissionHint") }}</p>
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
        <button class="link" @click="refreshPermission">{{ t("wizard.permissionRecheck") }}</button>
      </section>

      <!-- Step 2: which device triggers a switch and which one follows. -->
      <section v-if="screen === 'devices'" class="card">
        <p class="sub">{{ t("wizard.devicesHint") }}</p>
        <p v-if="scanning" class="muted">{{ t("scan.scanning") }}</p>
        <table v-else-if="found && found.length > 0">
          <thead>
            <tr>
              <th>{{ t("common.name") }}</th>
              <th class="center" style="width: 56px">{{ t("devices.trigger") }}</th>
              <th class="center" style="width: 56px">{{ t("devices.follow") }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="device in found" :key="device.id">
              <td>
                {{ device.name }}
                <span class="muted">
                  · {{ device.id }} ·
                  {{
                    t("devices.currentHost", {
                      channel: device.current_host + 1,
                      count: device.host_count,
                    })
                  }}
                </span>
              </td>
              <td class="center">
                <input
                  type="radio"
                  :checked="triggerId === device.id"
                  @change="pickTrigger(device)"
                />
              </td>
              <td class="center">
                <input type="radio" :checked="followId === device.id" @change="pickFollow(device)" />
              </td>
            </tr>
          </tbody>
        </table>
        <p v-else class="muted">{{ t("wizard.devicesNone") }}</p>

        <div v-if="channelOptions.length === 1" class="row">
          <span>{{ t("wizard.thisChannel", { channel: channelOptions[0] + 1 }) }}</span>
        </div>
        <template v-else-if="channelOptions.length > 1">
          <p class="sub">{{ t("wizard.channelDisagree") }}</p>
          <label v-for="channel in channelOptions" :key="channel" class="row">
            <span>
              <input v-model="channelPick" type="radio" :value="channel" />
              {{ t("overview.channelOnly", { channel: channel + 1 }) }}
            </span>
          </label>
        </template>

        <button class="link" :disabled="scanning" @click="scan">{{ t("devices.scan") }}</button>
      </section>

      <!-- Step 3: the machines sharing the keyboard. -->
      <section v-if="screen === 'machines'" class="card">
        <p class="sub">{{ t("wizard.machinesHint") }}</p>
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
            <tr v-for="(row, i) in hostRows" :key="i">
              <td><input v-model="row.name" type="text" /></td>
              <td class="num">
                <input
                  type="number"
                  min="1"
                  max="3"
                  :value="row.index + 1"
                  @input="setRowChannel(row, ($event.target as HTMLInputElement).value)"
                  @blur="($event.target as HTMLInputElement).value = String(row.index + 1)"
                />
              </td>
              <td class="center"><input v-model="thisRow" type="radio" :value="i" /></td>
              <td class="center">
                <button
                  class="mini"
                  :title="t('common.delete')"
                  :disabled="hostRows.length <= 2"
                  @click="removeMachine(i)"
                >
                  −
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <button class="link" :disabled="!canAddMachine" @click="addMachine">
          {{ t("hosts.add") }}
        </button>

        <!-- Only a three-machine setup has to say where a leave goes. -->
        <template v-if="needsLeaveTo">
          <h2 class="apart">{{ t("wizard.leaveTitle") }}</h2>
          <p class="sub">{{ t("wizard.leaveHint") }}</p>
          <label v-for="entry in leaveOptions" :key="entry.position" class="row">
            <span>
              <input v-model="leaveRow" type="radio" :value="entry.position" />
              {{ entry.row.name || t("common.unnamed") }}
              <span class="muted">
                · {{ t("overview.channelOnly", { channel: entry.row.index + 1 }) }}
              </span>
            </span>
          </label>
        </template>
      </section>

      <!-- Step 4: the shared display and each machine's input source. -->
      <section v-if="screen === 'displays'" class="card">
        <p class="sub">{{ t("wizard.displaysHint") }}</p>
        <p v-if="scanningDisplays" class="muted">{{ t("scan.scanning") }}</p>
        <template v-else>
          <p v-if="displays && displays.length === 0" class="muted">
            {{ t("wizard.displaysNone") }}
          </p>
          <label v-for="display in displays ?? []" :key="display.edid_uuid" class="row found">
            <span>
              <!-- Locked while this display is being asked its two questions:
                   a switch mid-read would mix the two monitors' answers. -->
              <input
                type="radio"
                :checked="chosenUuid === display.edid_uuid"
                :disabled="displayBusy"
                @change="chooseDisplay(display.edid_uuid)"
              />
              {{ display.name }}
            </span>
            <span class="v">
              <code :title="display.edid_uuid">{{ display.edid_uuid }}</code>
            </span>
          </label>
          <label v-if="displays && displays.length > 0" class="row">
            <span>
              <input
                type="radio"
                :checked="chosenUuid === null"
                :disabled="displayBusy"
                @change="chooseDisplay(null)"
              />
              {{ t("wizard.displaysSkip") }}
            </span>
          </label>
        </template>

        <template v-if="chosenDisplay">
          <h2 class="apart">{{ t("wizard.inputsTitle") }}</h2>
          <p v-if="loadingSources" class="sub">{{ t("wizard.inputsLoading") }}</p>
          <table>
            <tbody>
              <tr v-for="(row, i) in hostRows" :key="i">
                <td>
                  {{ row.name || t("common.unnamed") }}
                  <span class="muted">
                    ·
                    {{
                      i === thisRow
                        ? t("hosts.thisMac")
                        : t("overview.channelOnly", { channel: row.index + 1 })
                    }}
                  </span>
                </td>
                <td style="width: 190px">
                  <div class="input-cell">
                    <select
                      v-if="picks(row.index)"
                      :value="inputs[String(row.index)] ?? ''"
                      @change="choose(row.index, ($event.target as HTMLSelectElement).value)"
                    >
                      <option value="">{{ t("displays.inputEmpty") }}</option>
                      <option
                        v-for="source in optionsFor(row.index)"
                        :key="source.code"
                        :value="source.code"
                      >
                        {{ optionLabel(source) }}
                      </option>
                      <option value="custom">{{ t("displays.inputCustom") }}</option>
                    </select>
                    <template v-else>
                      <input
                        type="number"
                        min="0"
                        max="255"
                        :value="inputs[String(row.index)] ?? ''"
                        @input="setInput(row.index, ($event.target as HTMLInputElement).value)"
                      />
                      <button
                        v-if="sources.length > 0"
                        class="mini"
                        :title="t('displays.inputBackToList')"
                        @click="backToList(row.index)"
                      >
                        ↩
                      </button>
                    </template>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
          <!-- Only this Mac's row can be read: DDC reaches the display over
               this Mac's own cable, so the answer is this host's code. -->
          <button class="link" :disabled="displayBusy" @click="readThisInput()">
            {{ readingInput ? t("wizard.reading") : t("displays.readLong") }}
          </button>
        </template>
      </section>

      <!-- The end of both paths. -->
      <section v-if="screen === 'done'" class="card">
        <p class="sub">{{ t("wizard.doneHint") }}</p>
        <div v-if="offerExport" class="row">
          <span>{{ t("wizard.doneExportAsk") }}</span>
          <span class="v">
            <span v-if="exported" class="ok">{{ t("toast.exported") }}</span>
            <button class="mini" @click="exportConfig">{{ t("transfer.export") }}</button>
          </span>
        </div>
        <div class="row">
          <span class="muted">{{ t("wizard.doneNext") }}</span>
          <span class="v">
            <button class="primary" @click="emit('done')">{{ t("wizard.doneGo") }}</button>
          </span>
        </div>
      </section>
    </main>

    <footer v-if="screen !== 'choice' && screen !== 'done'">
      <button :disabled="!canBack || busy" @click="back">{{ t("wizard.back") }}</button>
      <span class="note muted" :title="footNote">{{ footNote }}</span>
      <button class="primary" :disabled="!canNext || busy" @click="next">
        {{ isLastStep ? t("wizard.finish") : t("wizard.next") }}
      </button>
    </footer>
  </div>
</template>

<style scoped>
/* The same three layers the settings window has: a header that names the step,
   a scrolling middle and a footer that never leaves. */
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
  align-items: baseline;
  justify-content: space-between;
  gap: 10px;
}

.title b {
  font-size: 15px;
  font-weight: 600;
}

/* The step counter and the way out, on one line opposite the title. */
.aside {
  flex: none;
  display: flex;
  align-items: baseline;
  gap: 10px;
}

.step {
  flex: none;
  font-size: 12px;
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

/* The fork: two targets big enough to be the only thing on the screen. */
.fork {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

button.big {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
  padding: 10px 12px;
  text-align: left;
}

button.big b {
  font-size: 14px;
  font-weight: 600;
}

button.big .muted {
  font-size: 11px;
}

/* A second heading inside a card, after a table or a list. */
.apart {
  margin-top: 14px;
}

/* The chosen file, which is a long path and may not fit. */
.path {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 11px;
  color: var(--muted);
}

/* A radio and its label read as one line, not as a control and a sentence. */
.row input[type="radio"] {
  width: auto;
  margin-right: 6px;
}

/* A display's EDID UUID is an identity, not a label: the name keeps its width
   and the uuid is the one that gives, down to an ellipsis and its tooltip. */
.found > span:first-child {
  flex: none;
}

.row .v {
  min-width: 0;
}

.row .v code {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 11px;
}

/* The input code and the buttons that stand beside it — the same row the
   settings window builds, so the number box and the ↩ button line up. */
.input-cell {
  display: flex;
  align-items: center;
  gap: 4px;
}

.input-cell select {
  width: 170px;
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

/* One line only: the banner has the room, this is the reminder next to the
   button it explains. */
.note {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  font-size: 11px;
  text-align: right;
}
</style>
