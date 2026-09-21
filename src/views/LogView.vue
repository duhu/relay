<script setup lang="ts">
// The core's ring buffer, newest last, polled every 2s.
import { getCurrentWindow } from "@tauri-apps/api/window";
import { nextTick, onMounted, onUnmounted, ref, watchEffect } from "vue";

import { setLanguage, t } from "../lib/i18n";
import { getLogs, getStatus, type LogEntry } from "../lib/ipc";

const REFRESH_MS = 2000;
/** How close to the bottom still counts as "following the tail". */
const STICK_PX = 24;

const entries = ref<LogEntry[]>([]);
const error = ref("");
const scroller = ref<HTMLElement | null>(null);

let timer = 0;
let unlisten: Array<() => void> = [];

// The native title bar is outside the Vue tree, so the window title has to be
// set from here; `t()` reads the language ref, so this re-runs whenever the
// language changes while the window is open. `windows.rs` gives the window a
// neutral title for the moment before this view loads.
watchEffect(() => {
  const title = t("log.title");
  getCurrentWindow()
    .setTitle(title)
    .catch(() => {
      /* a title the window manager refused is not worth showing */
    });
});

onMounted(() => {
  start();
  // The close button hides this window instead of destroying it, so the view
  // outlives being put away and the poll has to be stopped and started by hand:
  // `windows.rs` sends "hidden" as it hides the window and "shown" when it
  // comes back. Both are targeted at this window, which is why they are listened
  // for on it rather than on the app.
  const self = getCurrentWindow();
  void self.listen("hidden", () => stop()).then(remember).catch(cannotListen);
  void self.listen("shown", () => start()).then(remember).catch(cannotListen);
});

onUnmounted(() => {
  stop();
  for (const off of unlisten) off();
  unlisten = [];
});

function remember(off: () => void) {
  unlisten.push(off);
}

/**
 * A listener that could not be registered means this window stops following
 * the log the first time it is put away, and it has no other channel to say
 * so — the console is where a failure this quiet can still be found.
 */
function cannotListen(err: unknown) {
  console.error("cannot listen for this window being hidden or shown", err);
}

/** Refreshes now and keeps refreshing; safe to call on an already-running poll. */
function start() {
  stop();
  void refresh();
  timer = window.setInterval(refresh, REFRESH_MS);
}

function stop() {
  window.clearInterval(timer);
  timer = 0;
}

async function refresh() {
  const box = scroller.value;
  // Keep following the newest line unless the user scrolled up to read.
  const stick = !box || box.scrollHeight - box.scrollTop - box.clientHeight < STICK_PX;
  // This window shows no status, but the language lives on it — the same field
  // the tray reads — so following it here is what keeps this window in step
  // after the config is saved. A status that will not load leaves the language
  // as it was.
  try {
    setLanguage((await getStatus()).language);
  } catch {
    /* empty */
  }
  try {
    entries.value = await getLogs();
    error.value = "";
  } catch (err) {
    error.value = t("log.loadFailed", { detail: String(err) });
    return;
  }
  if (stick) {
    await nextTick();
    if (scroller.value) scroller.value.scrollTop = scroller.value.scrollHeight;
  }
}

function pad(value: number, width = 2): string {
  return String(value).padStart(width, "0");
}

function time(ts_ms: number): string {
  const at = new Date(ts_ms);
  return `${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}.${pad(
    at.getMilliseconds(),
    3,
  )}`;
}
</script>

<template>
  <main ref="scroller">
    <p v-if="error" class="error">{{ error }}</p>
    <p v-else-if="entries.length === 0" class="empty">{{ t("log.empty") }}</p>
    <table v-else>
      <tbody>
        <tr v-for="(entry, i) in entries" :key="i">
          <td class="ts">{{ time(entry.ts_ms) }}</td>
          <td class="level" :class="entry.level.toLowerCase()">{{ entry.level }}</td>
          <td class="target">{{ entry.target }}</td>
          <td class="message">{{ entry.message }}</td>
        </tr>
      </tbody>
    </table>
  </main>
</template>

<style scoped>
main {
  height: 100vh;
  overflow: auto;
  padding: 8px 10px;
  box-sizing: border-box;
  font-family: ui-monospace, "SF Mono", Menlo, monospace;
  font-size: 11px;
}

td {
  padding: 1px 6px 1px 0;
  border: 0;
  vertical-align: top;
  white-space: nowrap;
}

.ts,
.target {
  color: var(--muted);
}

.level {
  font-weight: 600;
}

.level.warn {
  color: var(--warn);
}

.level.error {
  color: var(--bad);
}

.message {
  width: 100%;
  white-space: pre-wrap;
  word-break: break-word;
}

.error {
  color: var(--bad);
}

.empty {
  color: var(--muted);
}
</style>
