import { createServer } from "node:http";
import { readFileSync, writeFileSync, mkdirSync, readdirSync } from "node:fs";
import { resolve, extname } from "node:path";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const root = resolve("views-demo/dist"), output = resolve("test-results/views");
mkdirSync(output, { recursive: true });
const assets = readdirSync(resolve(root, "assets")).sort();
const assetDigest = createHash("sha256");
for (const asset of assets) assetDigest.update(readFileSync(resolve(root, "assets", asset)));
const report = { source: "registry-generated fixtures with in-browser API simulation", rendererDigest: assetDigest.digest("hex"), applications: [], checks: [] };
const server = createServer((req, res) => {
  const path = new URL(req.url, "http://localhost").pathname;
  if (path.includes("..")) { res.writeHead(400).end(); return; }
  try {
    const file = readFileSync(resolve(root, "." + (path === "/" ? "/index.html" : path)));
    res.setHeader("Content-Type", extname(path) === ".js" ? "text/javascript" : extname(path) === ".css" ? "text/css" : "text/html"); res.end(file);
  } catch { res.writeHead(404).end(); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const origin = "http://127.0.0.1:" + server.address().port;
let browser;
const ID = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", REF = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
try {
  browser = await chromium.launch({ channel: "chrome", headless: true });
  report.browser = browser.version();
  for (const name of ["support", "maintenance"]) {
    const fixture = JSON.parse(readFileSync(resolve("../fixtures/views/" + name + ".json"), "utf8"));
    const views = fixture.admin.views;
    const primary = views.find((v) => v.kind === "collection" && v.filters.includes("state"));
    const recordView = views.find((v) => v.viewId === primary.recordView);
    const row = name === "support" ? { id: ID, subject: "Printer access request", account: REF, state: "open", assignee: null, version: 1, created_at: "2026-10-18T10:00:00Z" } : { id: ID, title: "Inspect cooling pump", asset: REF, state: "scheduled", notes: null, technician: null, version: 1, created_at: "2026-10-18T10:00:00Z" };
    const related = { id: REF, name: "North workshop", version: 1, created_at: row.created_at };
    const context = await browser.newContext({ viewport: { width: 1360, height: 900 } });
    const page = await context.newPage(), errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let empty = true, writes = 0, dropResponse = false;
    const json = (route, body, status = 200, headers = {}) => route.fulfill({ status, contentType: "application/json", headers, body: JSON.stringify(body) });
    await page.route("**/*", async (route) => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (path === "/_meta/v1/ui/manifest") return json(route, { protocol: "trestle.views", protocolVersion: "1.0", views: views.map((v) => ({ viewId: v.viewId, revision: v.revision, title: v.title, audience: v.audience, kind: v.kind, url: "/_meta/v1/ui/views/" + v.viewId })) });
      if (path.startsWith("/_meta/v1/ui/views/")) return json(route, views.find((v) => path.endsWith(v.viewId)));
      if (path.startsWith("/_meta/v1/admin/interactions/")) return json(route, fixture.adminForms.interactions.find((f) => path.endsWith(f.interactionId)));
      if (!path.startsWith("/api/")) return route.continue();
      if (request.method() === "GET") {
        if (path === primary.source.invocation.path) return json(route, { items: empty ? [] : [row], next_cursor: null });
        if (path === recordView.source.invocation.path.replace("{id}", ID)) return json(route, row, 200, { ETag: '"' + recordView.source.invocation.output.id + ':' + row.version + '"' });
        if (path.endsWith(REF)) return json(route, related);
        return json(route, { items: [related], next_cursor: null });
      }
      writes++;
      if (dropResponse) return route.abort("failed");
      if (request.method() === "PATCH") { Object.assign(row, request.postDataJSON()); row.version++; return json(route, row, 200, { ETag: '"' + recordView.source.invocation.output.id + ':' + row.version + '"' }); }
      return json(route, row, 201);
    });
    await page.goto(origin + "/#" + primary.viewId);
    await page.getByRole("heading", { name: "No records yet" }).waitFor();
    await page.getByRole("button", { name: /^New / }).click();
    await page.getByRole("dialog").waitFor();
    await page.getByRole("dialog").getByRole("textbox").first().waitFor();
    await page.getByRole("button", { name: "Close", exact: true }).click();
    empty = false; await page.reload();
    await page.getByRole("button", { name: /^Open / }).click();
    await page.getByRole("heading", { name: row.subject ?? row.title, exact: true }).waitFor();
    await page.screenshot({ path: resolve(output, name + "-record.png"), fullPage: true });
    await page.getByRole("button", { name: /^Edit / }).click();
    const textbox = page.getByRole("dialog").getByRole("textbox").first();
    await textbox.fill("Updated from generic admin");
    await page.getByRole("dialog").getByRole("button", { name: /^Save/ }).click();
    await page.getByRole("dialog").waitFor({ state: "hidden" });
    if (writes !== 1) throw new Error("Expected one edit dispatch");
    await page.getByRole("button", { name: /^Edit / }).click();
    await page.getByRole("dialog").getByRole("textbox").first().fill("Uncertain change");
    dropResponse = true;
    await page.getByRole("dialog").getByRole("button", { name: /^Save/ }).click();
    await page.getByRole("dialog").getByRole("alert").waitFor();
    await page.getByRole("button", { name: "Close", exact: true }).click();
    await page.getByRole("button", { name: /^Edit / }).click();
    if (!(await page.getByRole("dialog").getByRole("button", { name: /^Save/ }).isDisabled())) throw new Error("Unknown attempt became resubmittable");
    if (writes !== 2 || errors.length) throw new Error(JSON.stringify({ writes, errors }));
    report.applications.push({ name, result: "passed", writes });
    await context.close();
  }
  report.checks = ["same compiled asset digest", "empty-database navigation and create form", "record routing", "PATCH form", "unknown outcome survives dialog remount", "no duplicate dispatch", "no browser exceptions"];
  writeFileSync(resolve(output, "report.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report, null, 2));
} finally { await browser?.close(); await new Promise((r) => server.close(r)); }
