import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Fixed dev port: tauri.conf.json's devUrl points at it.
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5181,
    strictPort: true,
  },
  build: {
    target: "safari15",
  },
});
