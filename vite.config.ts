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
  build: {
    rollupOptions: {
      output: {
        // Split the heavyweights out of the entry chunk: less to parse before
        // first paint, and editing app code no longer invalidates vendor cache.
        manualChunks: {
          react: ["react", "react-dom"],
          codemirror: [
            "@codemirror/state",
            "@codemirror/view",
            "@codemirror/merge",
            "@codemirror/language",
            "@codemirror/language-data",
            "@codemirror/theme-one-dark",
          ],
          markdown: ["react-markdown", "remark-gfm"],
        },
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/sidecar/**"],
    },
  },
});
