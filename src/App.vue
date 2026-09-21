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
 * Bumped when the wizard finishes, to remount the settings view: it reads the
 * config once, on mount, and the file it would be showing is brand new.
 */
const settingsKey = ref(0);

const follow = () => {
  route.value = currentRoute();
  query.value = currentQuery();
};

onMounted(() => {
  window.addEventListener("hashchange", follow);
  void decide();
});

onUnmounted(() => window.removeEventListener("hashchange", follow));

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
    const status = await getStatus();
    // The core has already resolved `options.language: "auto"`, so the wizard
    // speaks the same language as the tray without asking again.
    setLanguage(status.language);
    needsWizard.value =
      !status.config_ok ||
      status.this_host === null ||
      !status.hosts.some(([index]) => index === status.this_host);
  } catch {
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
</script>

<template>
  <LogView v-if="route === 'log'" />
  <!-- Nothing at all until `decide()` answers: a blank window for a moment is
       better than the wrong one. -->
  <template v-else-if="needsWizard !== null">
    <WizardView
      v-if="route === 'wizard' || needsWizard"
      :key="query"
      :start-at-import="query === 'import'"
      @done="onWizardDone"
    />
    <SettingsView v-else :key="settingsKey" />
  </template>
</template>
