import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:3000",
        ws: true,
      },
      "/config": "http://127.0.0.1:3000",
      "/settings": "http://127.0.0.1:3000",
      "/command": "http://127.0.0.1:3000",
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
