/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev server port; fail fast instead of silently shifting.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  // Component tests need a DOM; everything else (store logic) runs faster without one, so
  // jsdom is opted into per file with `// @vitest-environment jsdom`.
  test: {
    environment: "node",
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/sidecar/**"],
    },
  },
});
