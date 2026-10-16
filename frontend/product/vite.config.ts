import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Same-origin deployment (design doc §12.2): the browser talks to one origin;
// /api and /_meta are proxied to the Rust service. No CORS needed.
const api = process.env.TRESTLE_API ?? "http://127.0.0.1:8000";
const proxy = { "/api": api, "/_meta": api };

export default defineConfig({
  plugins: [react()],
  server: { proxy },
  preview: { proxy },
  build: { outDir: "dist", emptyOutDir: true },
});
