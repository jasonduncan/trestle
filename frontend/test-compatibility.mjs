// Execute the actual pre-views decoder against both releases' descriptors.
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "vite";
import { decodeDescriptor } from "./client/dist/index.js";
const baseline = "a59936c";
const dir = mkdtempSync(join(tmpdir(), "trestle-old-renderer-"));
try {
  const source = execFileSync("git", ["show", baseline + ":frontend/interaction-react/src/protocol.ts"], { encoding: "utf8" });
  writeFileSync(join(dir, "protocol.ts"), source);
  await build({ configFile: false, root: dir, logLevel: "error", build: { lib: { entry: join(dir, "protocol.ts"), formats: ["es"], fileName: () => "decoder.mjs" }, outDir: join(dir, "dist") } });
  const old = await import(pathToFileURL(join(dir, "dist/decoder.mjs")).href);
  const files = ["interactions.json", "admin.json"];
  let legacy = 0, blocked = 0;
  for (const file of files) {
    const value = JSON.parse(readFileSync(resolve("../examples/pilot/published", file), "utf8"));
    for (const form of (Array.isArray(value.interactions) ? value.interactions : value.interactions.interactions)) {
      for (const decoder of [old.decodeDescriptor, decodeDescriptor]) {
        const result = decoder(form);
        if (!result.ok || result.descriptor.missingFeatures.length) throw new Error("Legacy form refused: " + form.interactionId);
      }
      legacy++;
    }
  }
  for (const name of ["support", "maintenance"]) {
    const fixture = JSON.parse(readFileSync(resolve("../fixtures/views/" + name + ".json"), "utf8"));
    for (const form of [...fixture.adminForms.interactions, ...fixture.productForms.interactions]) {
      const current = decodeDescriptor(form);
      if (!current.ok || current.descriptor.missingFeatures.length) throw new Error("Current form refused: " + form.interactionId);
      if (form.protocolVersion === "1.1") {
        const prior = old.decodeDescriptor(form);
        if (prior.ok && !prior.descriptor.missingFeatures.length) throw new Error("Old renderer silently accepts required new semantics");
        blocked++;
      }
    }
  }
  if (!blocked || !legacy) throw new Error("Empty compatibility coverage");
  mkdirSync("test-results", { recursive: true });
  const report = { baseline, legacyFormsAcceptedByBoth: legacy, extendedFormsSafelyBlockedByOld: blocked, result: "passed" };
  writeFileSync("test-results/compatibility.json", JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} finally { rmSync(dir, { recursive: true, force: true }); }
