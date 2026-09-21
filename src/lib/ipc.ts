// Typed wrappers around the Tauri commands in `src-tauri/src/app/commands.rs`.
// The types mirror the Rust structs by hand; keep them in step with
// `relay_core::config`, `relay_core::runtime::Status` and `relay_core::executor`.

import { invoke } from "@tauri-apps/api/core";

/** A host slot, 0-based, exactly as HID++ numbers them; the UI shows index + 1. */
export type HostIndex = number;

export type DeviceRole = "keyboard" | "mouse" | "other";

export interface Host {
  index: HostIndex;
  name: string;
}

export interface DisplayConfig {
  edid_uuid: string;
  name: string;
  /** Host index as a string, e.g. `"0"`, to its DDC input source. */
  input_by_host: Record<string, number>;
}

export interface DeviceConfig {
  id: string;
  name: string;
  role: DeviceRole;
  transport: string;
  /**
   * Tells two devices with the same `vid:pid` apart. Optional: an old config
   * simply has no such key, and the backend skips it on save when it is unset.
   */
  serial?: string | null;
  is_trigger: boolean;
  follow: boolean;
  leave_to: HostIndex | null;
}

export interface Timing {
  debounce_ms: number;
  cooldown_ms: number;
  ddc_retries: number;
}

/** `options.language`; `"auto"` follows the system, resolved by relay-core. */
export type Language = "auto" | "zh-Hans" | "en";

export interface Options {
  switch_back_on_reconnect: boolean;
  pull_on_arrival: boolean;
  launch_at_login: boolean;
  language: Language;
}

export interface Config {
  schema_version: number;
  this_host: HostIndex;
  hosts: Host[];
  displays: DisplayConfig[];
  devices: DeviceConfig[];
  timing: Timing;
  hotkeys: Record<string, string>;
  options: Options;
}

export interface StepResult {
  what: string;
  ok: boolean;
  detail: string;
  ms: number;
}

export interface SwitchReport {
  target: HostIndex;
  steps: StepResult[];
  ok: boolean;
}

export interface Status {
  state: string;
  config_ok: boolean;
  config_error: string | null;
  this_host: HostIndex | null;
  hosts: [HostIndex, string][];
  input_monitoring: boolean;
  last_report: SwitchReport | null;
  /**
   * The concrete language the window and the tray must speak, `"zh-Hans"` or
   * `"en"`. Never `"auto"`: relay-core resolves that, so both UIs agree.
   */
  language: string;
}

export interface LogEntry {
  ts_ms: number;
  level: string;
  target: string;
  message: string;
}

export interface HidDeviceInfo {
  vid: string;
  pid: string;
  /** The `vid:pid` form a device's `id` uses. */
  id: string;
  name: string;
}

/** One switchable Logitech device found by `scan_devices`. */
export interface DiscoveredDevice {
  /** The `vid:pid` form a device's `id` uses. */
  id: string;
  /** `null` when neither the device nor its HID node reports a serial. */
  serial: string | null;
  name: string;
  host_count: number;
  /** 0-based, exactly as HID++ numbers the slots; the UI shows index + 1. */
  current_host: number;
  role_guess: DeviceRole;
}

/** One external display found by `list_displays`. */
export interface DiscoveredDisplay {
  /** The display's identity in the config; compared case-insensitively. */
  edid_uuid: string;
  /** `Unknown Display` when the framebuffer advertises no product name. */
  name: string;
}

/** Reads the config file; rejects when it is missing or unparseable. */
export function getConfig(): Promise<Config> {
  return invoke<Config>("get_config");
}

/** Validates and writes the config; rejects with the validation message. */
export function saveConfig(cfg: Config): Promise<void> {
  return invoke<void>("save_config", { cfg });
}

export function getStatus(): Promise<Status> {
  return invoke<Status>("get_status");
}

/** A manual switch to `target`, skipping the debounce. */
export function triggerSwitch(target: HostIndex): Promise<SwitchReport> {
  return invoke<SwitchReport>("trigger_switch", { target });
}

export function getLogs(): Promise<LogEntry[]> {
  return invoke<LogEntry[]>("get_logs");
}

export function listHidDevices(): Promise<HidDeviceInfo[]> {
  return invoke<HidDeviceInfo[]>("list_hid_devices");
}

/**
 * Probes every HID++ node for `ChangeHost` (0x1814). Needs Input Monitoring
 * and takes seconds; a device that will not answer is simply absent.
 */
export function scanDevices(): Promise<DiscoveredDevice[]> {
  return invoke<DiscoveredDevice[]>("scan_devices");
}

/**
 * Walks the IORegistry for every external display this Mac can drive over
 * DDC. Needs no permission and takes milliseconds.
 */
export function listDisplays(): Promise<DiscoveredDisplay[]> {
  return invoke<DiscoveredDisplay[]>("list_displays");
}

/**
 * Asks one display which input source it is showing. `edidUuid` is empty for
 * the first external display. Resolves to `null` when the display will not
 * answer — that is an answer, not a failure, so the caller falls back to the
 * code the user types in.
 */
export function readDisplayInput(edidUuid: string): Promise<number | null> {
  return invoke<number | null>("read_display_input", { edidUuid });
}

export function inputMonitoringGranted(): Promise<boolean> {
  return invoke<boolean>("input_monitoring_granted");
}

/** Shows the system prompt and resolves to whether it was granted. */
export function requestInputMonitoring(): Promise<boolean> {
  return invoke<boolean>("request_input_monitoring");
}

export function openPrivacySettings(): Promise<void> {
  return invoke<void>("open_privacy_settings");
}
