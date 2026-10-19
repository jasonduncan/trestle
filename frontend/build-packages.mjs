import { build } from "vite";
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync, copyFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";

for (const name of ["client", "interaction-react", "admin-react"]) {
  const root = resolve(name);
  const entries = name === "client" ? Object.fromEntries(["index", "protocol", "transport", "validate", "scalar"].map((entry) => [entry, resolve(root, "src", entry + ".ts")])) : resolve(root, "src/index." + (name === "admin-react" ? "tsx" : "ts"));
  await build({ configFile: false, root, build: {
    lib: { entry: entries, formats: ["es"], fileName: (format, name) => name + ".js" },
    rollupOptions: { external: (id) => id === "react" || id.startsWith("react/") || id === "react-dom" || id.startsWith("@trestle/") },
    minify: false, sourcemap: true,
  } });
  const result = spawnSync(process.execPath, [resolve("node_modules/typescript/bin/tsc"), "-p", resolve(root, "tsconfig.build.json")], { stdio: "inherit" });
  if (result.status !== 0) process.exit(result.status ?? 1);
  if (name === "admin-react") {
    copyFileSync(resolve(root, "src/styles.css"), resolve(root, "dist/styles.css"));
    writeFileSync(resolve(root, "dist/styles.d.css.ts"), "export {};\n");
  }
}
const { createHash } = await import("node:crypto");
const { SUPPORTED_FEATURES, VIEW_FEATURES } = await import("./client/dist/index.js");
const manifest = {
  protocol: "trestle.renderer", protocolVersion: "1.0",
  packages: Object.fromEntries(["client", "interaction-react", "admin-react"].map((name) => {
    const pkg = JSON.parse(readFileSync(resolve(name, "package.json"), "utf8")); return [pkg.name, pkg.version];
  })),
  viewVersions: ["1.0"], interactionVersions: ["1.0", "1.1"],
  features: [...SUPPORTED_FEATURES, ...VIEW_FEATURES].sort(), components: {},
};
const assetHash = createHash("sha256");
for (const name of ["client", "interaction-react", "admin-react"]) {
  for (const file of readdirSync(resolve(name, "dist")).filter((f) => /\.(js|css)$/.test(f)).sort()) {
    assetHash.update(name + "/" + file + "\0"); assetHash.update(readFileSync(resolve(name, "dist", file)));
  }
}
manifest.digest = assetHash.update(JSON.stringify(manifest)).digest("hex");
writeFileSync("admin-react/dist/renderer.json", JSON.stringify(manifest, null, 2) + "\n");
