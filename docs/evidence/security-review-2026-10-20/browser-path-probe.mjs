// Run from the repository root after the frontend package build.
// Asserts the vulnerable behavior at the reviewed commit; sends no requests.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { decodeDescriptor } from "../../../frontend/client/dist/protocol.js";
import { fillPath } from "../../../frontend/client/dist/transport.js";

const fixture = JSON.parse(readFileSync(new URL(
  "../../../fixtures/interactions/intent-crm-deal-create.json", import.meta.url
), "utf8"));
const origin = "https://app.example.com";
for (const [path, expected] of [
  ["/api/%2e%2e/admin/", "/admin/"],
  ["/api/%2E%2E/%2E%2E/metrics", "/metrics"],
]) {
  const descriptor = structuredClone(fixture);
  descriptor.operation.binding.path = path;
  const decoded = decodeDescriptor(descriptor);
  assert.equal(decoded.ok, true);
  const resolved = new URL(path, origin).pathname;
  assert.equal(resolved, expected);
  console.log(JSON.stringify({ path, accepted: decoded.ok, resolved }));
}
const filled = fillPath("/api/{id}/actions/run", { id: ".." });
const resolved = new URL(filled, origin).pathname;
assert.equal(resolved, "/actions/run");
console.log(JSON.stringify({ template: "/api/{id}/actions/run", id: "..", filled, resolved }));
