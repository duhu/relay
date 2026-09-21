<script setup lang="ts">
// One bundle, three views: the Rust side opens `index.html#/settings` or
// `index.html#/log` and this picks the view from the hash. `#/wizard` is the
// window's own doing — the settings view sets it, and so does a machine that
// has never been configured.
import { onMounted, onUnmounted, ref } from "vue";

import { setLanguage } from "./lib/i18n";
import { getStatus } from "./lib/ipc";
import LogView from "./views/LogView.vue";
import SettingsView from "./views/SettingsView.vue";
import WizardView from "./views/WizardView.vue";

function currentRoute(): string {
  return window.location.hash.replace(/^#\/?/, "").split("?")[0] || "settings";
}

/** `#/wizard?import` — the settings view's import button, which skips the fork. */
function currentQuery(): string {
  return window.location.hash.split("?")[1] ?? "";
}

const route = ref(currentRoute());
const query = ref(currentQuery());
/**
 * Whether this machine needs the wizard rather than the settings form. `null`
 * until the status has answered: neither view may be rendered before then, or
 * a machine that has never been configured would flash the empty form first.
 */
const needsWizard = ref<boolean | null>(null);
/**
 * The user left the wizard without finishing it. It says nothing about whether
 * the machine is configured — only that this window shows the settings form for
 * the rest of the session, which is the wizard's only way out on a machine
 * where the wizard is the only view.
 */
const leftWizard = ref(false);
/**
 * Bumped when the wizard finishes, to remount the settings view: it reads the
 * config once, on mount, and the file it would be showing is brand new.
 */
const settingsKey = ref(0);

const follow = () => {
  route.value = currentRoute();
  query.value = currentQuery();
  // Asking for the wizard by name — the Advanced tab's button — takes back an
  // earlier leave; otherwise that button would do nothing for the session.
  if (route.value === "wizard") leftWizard.value = false;
};

onMounted(() => {
  window.addEventListener("hashchange", follow);
  void decide();
});

onUnmounted(() => window.removeEventListener("hashchange", follow));

/** How long the window waits for the status before it stops waiting for it. */
const STATUS_TIMEOUT_MS = 4000;

/**
 * What language to speak when the status never answered, which is exactly the
 * path the wizard appears on — so without this a first run on an English Mac
 * would come up in Chinese, the dictionary's fallback.
 *
 * The rule is `tag_language` in `crates/relay-core/src/config.rs`, spelled the
 * same way here: anything Chinese, in any script or region, is the Simplified
 * dictionary, and everything else is English.
 */
function systemLanguage(): string {
  return (navigator.language ?? "").trim().toLowerCase().startsWith("zh") ? "zh-Hans" : "en";
}

/**
 * Asks the core once whether this machine is configured at all.
 *
 * Unconfigured is exactly what the tray means by it: the config did not load,
 * or `this_host` names a machine the file does not declare. A status that does
 * not come back at all is the wizard's case too — a core that will not answer
 * is not a core whose settings are worth editing.
 */
async function decide() {
  try {
    // A status that never settles would leave this window blank forever, which
    // is worse than the wizard on a machine that did not need it.
    const status = await Promise.race([
      getStatus(),
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("status timed out")), STATUS_TIMEOUT_MS),
      ),
    ]);
    // The core has already resolved `options.language: "auto"`, so the wizard
    // speaks the same language as the tray without asking again.
    setLanguage(status.language);
    needsWizard.value =
      !status.config_ok ||
      status.this_host === null ||
      !status.hosts.some(([index]) => index === status.this_host);
  } catch {
    // No status means no resolved language either; the system's own preference
    // is the best guess left.
    setLanguage(systemLanguage());
    needsWizard.value = true;
  }
}

/**
 * The wizard wrote a config; this window is a settings window again. The hash
 * goes back so a reopen lands on the settings view, and the key change makes
 * that view read the file the wizard just wrote.
 */
function onWizardDone() {
  needsWizard.value = false;
  settingsKey.value += 1;
  window.location.hash = "#/settings";
  // `hashchange` fires asynchronously, and when the hash was already
  // `#/settings` it does not fire at all.
  follow();
}

/**
 * The user left the wizard without it writing anything. The machine may still
 * be unconfigured — the settings form is where it gets filled in by hand — so
 * `needsWizard` keeps its meaning and this window simply stops showing the
 * wizard. The hash goes back too, because closing the window only hides it: a
 * reopen keeps this webview and would otherwise land on the wizard again.
 */
function onWizardExit() {
  leftWizard.value = true;
  window.location.hash = "#/settings";
  follow();
}
</script>

<template>
  <LogView v-if="route === 'log'" />
  <!-- Nothing at all until `decide()` answers: a blank window for a moment is
       better than the wrong one. -->
  <template v-else-if="needsWizard !== null">
    <WizardView
      v-if="!leftWizard && (route === 'wizard' || needsWizard)"
      :key="query"
      :start-at-import="query === 'import'"
      :only-view="needsWizard === true"
      @done="onWizardDone"
      @exit="onWizardExit"
    />
    <SettingsView v-else :key="settingsKey" />
  </template>
</template>
