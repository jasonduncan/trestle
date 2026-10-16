import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

/** Reads a JSON file relative to the repository root. */
export function repoJson<T = any>(relative: string): T {
  return JSON.parse(readFileSync(resolve(repoRoot, relative), "utf8")) as T;
}

/** The descriptor the pilot backend actually publishes (committed snapshot). */
export function publishedDescriptor(id = "lessons.create.form"): any {
  const exported = repoJson("examples/pilot/published/interactions.json");
  const descriptor = exported.interactions.find((d: any) => d.interactionId === id);
  if (!descriptor) throw new Error(`no published descriptor ${id}`);
  return structuredClone(descriptor);
}

/** The admin edit descriptor the backend publishes. */
export function publishedAdminDescriptor(id = "lessons.admin_edit.form"): any {
  const admin = repoJson("examples/pilot/published/admin.json");
  const descriptor = admin.interactions.interactions.find((d: any) => d.interactionId === id);
  if (!descriptor) throw new Error(`no admin descriptor ${id}`);
  return structuredClone(descriptor);
}
