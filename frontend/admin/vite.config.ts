import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Same-origin deployment (design doc §12.2): the browser talks to one origin;
// /api and /_meta are proxied to the Rust service. No CORS needed. The admin
// shell lives under /admin/ on that origin, so it shares the host-only
// session cookie and the login callback with the product app.
const api = process.env.TRESTLE_API ?? "http://127.0.0.1:8000";
const proxy = { "/api": api, "/_meta": api };

export default defineConfig({
  base: "/admin/",
  server: { port: 5174, proxy },
  plugins: [react()],

  preview: { proxy },
  build: { outDir: "dist", emptyOutDir: true },
});
