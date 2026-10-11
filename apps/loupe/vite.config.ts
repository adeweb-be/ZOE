import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri loads the dev server at a fixed port and the build from dist/.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "dist",
  },
});
