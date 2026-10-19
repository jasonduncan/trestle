import { defineConfig } from "vite";
export default defineConfig({ server: { port: 5174, proxy: {
  "/api": "http://localhost:8000", "/_meta": "http://localhost:8000", "/auth": "http://localhost:8000",
} } });
