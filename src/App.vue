<script setup lang="ts">
// One bundle, two windows: the Rust side opens `index.html#/settings` or
// `index.html#/log` and this picks the view from the hash.
import { onMounted, onUnmounted, ref } from "vue";

import LogView from "./views/LogView.vue";
import SettingsView from "./views/SettingsView.vue";

function currentRoute(): string {
  return window.location.hash.replace(/^#\/?/, "") || "settings";
}

const route = ref(currentRoute());
const follow = () => {
  route.value = currentRoute();
};

onMounted(() => window.addEventListener("hashchange", follow));
onUnmounted(() => window.removeEventListener("hashchange", follow));
</script>

<template>
  <LogView v-if="route === 'log'" />
  <SettingsView v-else />
</template>
