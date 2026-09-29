<script setup lang="ts">
// The mouse-buttons card on the Keyboard & mouse tab: what the thumb gesture
// button, back and forward press instead. It edits `mouse_buttons` through
// v-model and never saves on its own — the window's save button does that.
import { onMounted, onUnmounted, ref } from "vue";

import { t } from "../lib/i18n";
import * as ipc from "../lib/ipc";
import type { ButtonAction, KeyCombo, MouseButtonMapping, Preset } from "../lib/ipc";
import { keycaps, recordKey } from "../lib/keys";

const props = defineProps<{ modelValue: MouseButtonMapping[] }>();
const emit = defineEmits<{ "update:modelValue": [rows: MouseButtonMapping[]] }>();

/** CGEvent `buttonNumber` of each row, 0-based; left and right (0, 1) never appear. */
const ROWS = [
  { button: 6, label: "buttons.gesture", none: "buttons.none" },
  { button: 3, label: "buttons.back", none: "buttons.noneBack" },
  { button: 4, label: "buttons.forward", none: "buttons.noneForward" },
] as const;

const PRESETS: Preset[] = [
  "mission_control",
  "app_windows",
  "show_desktop",
  "space_left",
  "space_right",
];

/** The row waiting for a shortcut to be pressed; only one at a time. */
const recording = ref<number | null>(null);
/** `null` until the first answer, so the strip does not flash on open. */
const axGranted = ref<boolean | null>(null);
let axTimer = 0;
/** Set on unmount, so a permission answer arriving late starts no timer. */
let disposed = false;

function actionOf(button: number): ButtonAction | null {
  return props.modelValue.find((m) => m.button === button)?.action ?? null;
}

function comboOf(button: number): KeyCombo | null {
  const action = actionOf(button);
  return action && "keys" in action ? action.keys : null;
}

/** The dropdown's value: "none", a preset id, or "custom". */
function choice(button: number): string {
  if (recording.value === button) return "custom";
  const action = actionOf(button);
  if (!action) return "none";
  return "preset" in action ? action.preset : "custom";
}

/** Emits a new list with `button` set to `action`, or removed for `null`. */
function write(button: number, action: ButtonAction | null) {
  const rows = props.modelValue;
  let next: MouseButtonMapping[];
  if (action === null) {
    next = rows.filter((m) => m.button !== button);
  } else if (rows.some((m) => m.button === button)) {
    next = rows.map((m) => (m.button === button ? { button, action } : m));
  } else {
    next = [...rows, { button, action }];
  }
  emit("update:modelValue", next);
}

function choose(button: number, value: string) {
  if (value === "custom") {
    // A shortcut already recorded stays; otherwise the next keys pressed are it.
    if (!comboOf(button)) startRecording(button);
    return;
  }
  if (recording.value === button) stopRecording();
  write(button, value === "none" ? null : { preset: value as Preset });
}

function startRecording(button: number) {
  if (recording.value === null) {
    window.addEventListener("keydown", onKeydown, true);
    window.addEventListener("pointerdown", onPointerdown, true);
    window.addEventListener("blur", stopRecording);
  }
  recording.value = button;
}

/**
 * Ends recording without writing anything: the row falls back to whatever the
 * config held before, which is what Esc, a click elsewhere and leaving the
 * window all want.
 */
function stopRecording() {
  recording.value = null;
  window.removeEventListener("keydown", onKeydown, true);
  window.removeEventListener("pointerdown", onPointerdown, true);
  window.removeEventListener("blur", stopRecording);
}

// A press anywhere but the recording row's own dropdown and button gives the
// keyboard back, so the rest of the window is usable without reaching for Esc.
function onPointerdown(e: PointerEvent) {
  const own = (e.target as Element | null)?.closest?.("[data-button]");
  if (own?.getAttribute("data-button") !== String(recording.value)) stopRecording();
}

// Capture phase on window, so the focused dropdown never sees the keys: an
// arrow key must become part of the shortcut, not move the selection.
function onKeydown(e: KeyboardEvent) {
  const button = recording.value;
  if (button === null) return;
  e.preventDefault();
  e.stopPropagation();
  const result = recordKey(e);
  if (result === "ignore") return;
  stopRecording();
  if (result !== "cancel") write(button, { keys: result.combo });
}

async function checkAccessibility() {
  if (disposed) return;
  try {
    axGranted.value = await ipc.accessibilityGranted();
  } catch (err) {
    console.error("cannot read the Accessibility state", err);
  }
  if (disposed) return;
  if (axGranted.value === false) {
    if (!axTimer) axTimer = window.setInterval(() => void checkAccessibility(), 2000);
  } else if (axTimer) {
    window.clearInterval(axTimer);
    axTimer = 0;
  }
}

async function grant() {
  try {
    if (await ipc.requestAccessibility()) void checkAccessibility();
  } catch (err) {
    console.error("cannot ask for Accessibility", err);
  }
}

onMounted(() => void checkAccessibility());

onUnmounted(() => {
  disposed = true;
  stopRecording();
  window.clearInterval(axTimer);
  axTimer = 0;
});
</script>

<template>
  <section class="card">
    <h2>{{ t("buttons.title") }}</h2>
    <p class="sub">{{ t("buttons.sub") }}</p>
    <div v-if="axGranted === false" class="perm">
      <span>{{ t("buttons.permissionMissing") }}</span>
      <button class="mini" @click="grant">{{ t("buttons.grant") }}</button>
    </div>
    <div v-for="row in ROWS" :key="row.button" class="row">
      <span>{{ t(row.label) }}</span>
      <span class="v" :data-button="row.button">
        <select
          :value="choice(row.button)"
          @change="choose(row.button, ($event.target as HTMLSelectElement).value)"
        >
          <option value="none">{{ t(row.none) }}</option>
          <optgroup :label="t('buttons.presets')">
            <option v-for="preset in PRESETS" :key="preset" :value="preset">
              {{ t(`buttons.preset.${preset}`) }}
            </option>
          </optgroup>
          <option value="custom">{{ t("buttons.custom") }}</option>
        </select>
        <button v-if="recording === row.button" class="mini rec" @click="stopRecording">
          {{ t("buttons.recording") }}
        </button>
        <span
          v-else-if="comboOf(row.button)"
          class="keys"
          @click="startRecording(row.button)"
        >
          <kbd v-for="(cap, i) in keycaps(comboOf(row.button)!)" :key="i">{{ cap }}</kbd>
        </span>
      </span>
    </div>
  </section>
</template>

<style scoped>
/* Wide enough that the longest English option, "Unchanged (browser forward)",
   is never clipped. Rows showing keycaps still sit their dropdown further left. */
select {
  width: 220px;
}

/* A recorded shortcut; clicking any cap records it again. */
.keys {
  display: inline-flex;
  gap: 4px;
  cursor: pointer;
}

kbd {
  font: 12px -apple-system, BlinkMacSystemFont, sans-serif;
  color: var(--text);
  min-width: 22px;
  text-align: center;
  white-space: nowrap;
  padding: 1px 6px;
  border: 1px solid var(--fieldline);
  border-bottom-width: 2px;
  border-radius: 5px;
  background: var(--field);
}

.rec {
  border-color: var(--accent);
  color: var(--accent);
}

/* Warning, not failure: the mappings are kept, they just do nothing yet. */
.perm {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-bottom: 4px;
  padding: 8px 10px;
  border-radius: 8px;
  background: color-mix(in srgb, var(--warn) 12%, transparent);
  color: var(--warn);
  font-size: 12px;
}
</style>
