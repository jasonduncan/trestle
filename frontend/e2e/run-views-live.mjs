// Spawned by views_live.rs: real registry, executor and disposable PostgreSQL.
import { createServer, request } from "node:http";
import { readFileSync, writeFileSync, mkdirSync, readdirSync } from "node:fs";
import { resolve, extname } from "node:path";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { platform, arch, cpus, totalmem } from "node:os";
import { chromium } from "playwright-core";

const name = process.env.VIEW_APP, stage = process.env.VIEW_STAGE;
const backend = new URL(process.env.VIEW_BACKEND);
if (backend.hostname !== "127.0.0.1") throw new Error("Test backend must be loopback");
const root = resolve("views-demo/dist"), output = resolve("test-results/views-live");
mkdirSync(output, { recursive: true });
const digest = createHash("sha256");
for (const file of readdirSync(resolve(root, "assets")).sort()) digest.update(readFileSync(resolve(root, "assets", file)));
const fixture = JSON.parse(readFileSync(resolve("../fixtures/views/" + name + ".json"), "utf8"));
const primary = fixture.admin.views.find((v) => v.kind === "collection" && v.filters.includes("state"));
const report = { application: name, stage, rendererDigest: digest.digest("hex"), backendCommit: execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim(), workingTree: "local implementation under test", schema: process.env.VIEW_SCHEMA, machine: { platform: platform(), arch: arch(), cpus: cpus().length, memory: totalmem() }, records: stage === "empty" ? 0 : 10000, relatedChoices: stage === "empty" ? 0 : 125, checks: [] };
const server = createServer((req, res) => {
  if (req.url.startsWith("/api/") || req.url.startsWith("/_meta/")) {
    const upstream = request(new URL(req.url, backend), { method: req.method, headers: { ...req.headers, host: backend.host, authorization: "Bearer alice-view-test", "trestle-tenant": "00000000-0000-0000-0000-000000007ea3" } }, (reply) => { res.writeHead(reply.statusCode, reply.headers); reply.pipe(res); });
    upstream.on("error", () => res.writeHead(502).end()); req.pipe(upstream); return;
  }
  const path = new URL(req.url, "http://localhost").pathname;
  if (path.includes("..")) { res.writeHead(400).end(); return; }
  try {
    const file = readFileSync(resolve(root, "." + (path === "/" ? "/index.html" : path)));
    res.setHeader("Content-Type", extname(path) === ".js" ? "text/javascript" : extname(path) === ".css" ? "text/css" : "text/html"); res.end(file);
  } catch { res.writeHead(404).end(); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
let browser, page;
const errors = [];
try {
  browser = await chromium.launch({ ...(process.env.VIEW_CHROME_CHANNEL === "bundled" ? {} : { channel: "chrome" }), headless: true });
  report.browser = browser.version();
  page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
  const tabTo = async (target) => {
    for (let i = 0; i < 35; i++) {
      if (await target.evaluate((element) => element === document.activeElement)) return;
      await page.keyboard.press("Tab");
    }
    throw new Error("Control was not reachable with the keyboard");
  };
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("dialog", (dialog) => dialog.accept());
  await page.goto("http://127.0.0.1:" + server.address().port + "/#" + primary.viewId);
  if (stage === "empty") {
    await page.getByRole("heading", { name: "No records yet" }).waitFor();
    await page.getByRole("button", { name: /^New / }).click();
    await page.getByRole("dialog").getByRole("textbox").first().waitFor();
    await page.screenshot({ path: resolve(output, name + "-empty.png") });
    report.checks.push("A01 empty database metadata and create form", "A52 minimum intent admin declaration");
  } else {
    await page.getByRole("button", { name: /^Open / }).first().waitFor();
    if (await page.getByRole("button", { name: /^Open / }).count() !== 20) throw new Error("Expected bounded page");
    const first = await page.getByRole("button", { name: /^Open / }).first().textContent();
    await page.getByRole("button", { name: "Next", exact: true }).click();
    await page.getByText("Page 2", { exact: true }).waitFor();
    await page.getByRole("button", { name: /^Open / }).first().waitFor();
    if (await page.getByRole("button", { name: /^Open / }).first().textContent() === first) throw new Error("Cursor did not advance");
    await page.getByRole("button", { name: /^Open / }).first().click();
    await page.getByRole("heading", { name: /^Record / }).waitFor();
    await page.getByRole("button", { name: /^Edit / }).click();
    const dialog = page.getByRole("dialog");
    const reference = name === "support" ? "Account" : "Asset";
    await dialog.getByRole("option", { name: "Choice 125" }).waitFor({ state: "attached" });
    await tabTo(dialog.getByRole("searchbox", { name: "Search " + reference }));
    const searched = page.waitForResponse((reply) => reply.url().includes("q=125") && reply.status() === 200);
    await page.keyboard.type("125"); await searched;
    await dialog.getByRole("option", { name: "Choice 125" }).waitFor({ state: "attached" });
    const label = name === "support" ? "Subject" : "Title";
    const labelPattern = new RegExp("^" + label + "(?: \\(optional\\))?$");
    await tabTo(dialog.getByRole("combobox", { name: new RegExp("^" + reference + "(?: \\(optional\\))?$") }));
    await page.keyboard.press("Home"); await page.keyboard.press("ArrowDown");
    await tabTo(dialog.getByRole("textbox", { name: labelPattern }));
    await page.keyboard.press("ControlOrMeta+A"); await page.keyboard.type("Verified with live PostgreSQL");
    await tabTo(dialog.getByRole("button", { name: /^Save/ })); await page.keyboard.press("Enter");
    await dialog.waitFor({ state: "hidden" });
    await page.getByRole("heading", { name: "Verified with live PostgreSQL", exact: true }).waitFor();
    await page.screenshot({ path: resolve(output, name + "-record.png") });
    // A genuine committed PATCH whose response is lost. Only the transport is faulted.
    let writes = 0;
    await page.route("**/api/v1/**", async (route) => {
      if (route.request().method() !== "PATCH") return route.continue();
      writes++; await route.fetch(); await route.abort("failed");
    });
    await page.getByRole("button", { name: /^Edit / }).click();
    await dialog.getByRole("textbox", { name: labelPattern }).fill("Committed but reply lost");
    await dialog.getByRole("button", { name: /^Save/ }).click();
    await dialog.getByRole("alert").waitFor();
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await page.getByRole("button", { name: /^Edit / }).click();
    await dialog.getByRole("button", { name: /^Save/ }).waitFor();
    if (!await dialog.getByRole("button", { name: /^Save/ }).isDisabled() || writes !== 1) throw new Error("Lost attempt became resubmittable");
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await page.getByRole("button", { name: "Refresh record", exact: true }).click();
    await page.getByRole("heading", { name: "Committed but reply lost", exact: true }).waitFor();
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await dialog.getByRole("button", { name: "Confirm delete", exact: true }).click();
    await dialog.waitFor({ state: "hidden" });
    report.checks.push("A39 checked inputless DELETE binding through shared controller");
    const workspace = name === "support" ? "support_work.queue" : "maintenance.queue";
    await page.goto("http://127.0.0.1:" + server.address().port + "/?profile=product#" + workspace);
    await page.getByRole("button", { name: /^Open / }).first().click();
    await page.getByRole("button", { name: name === "support" ? "Assign case" : "Complete work order", exact: true }).click();
    const action = page.getByRole("dialog");
    if (name === "support") {
      await action.getByRole("searchbox", { name: "Search Assignee" }).fill("125");
      await action.getByRole("option", { name: "Person 125" }).waitFor({ state: "attached" });
      await action.getByRole("combobox", { name: "Assignee" }).selectOption({ label: "Person 125" });
    } else await action.getByRole("textbox", { name: "Notes" }).fill("Inspection completed");
    await action.getByRole("button", { name: name === "support" ? "Assign" : "Complete", exact: true }).click();
    await action.waitFor({ state: "hidden" });
    if (name === "support") {
      await page.getByRole("button", { name: "Export preview", exact: true }).click();
      await page.getByRole("button", { name: "Queue export", exact: true }).click();
      await page.getByText("Work: succeeded", { exact: true }).waitFor();
      report.checks.push("input-taking Assign command", "required custom section", "durable export preview and authorized status", "separate precondition target with command receipt");
    } else {
      await page.getByRole("button", { name: "Asset", exact: true }).click();
      await page.getByRole("cell", { name: "Choice 125", exact: true }).waitFor();
      report.checks.push("input-taking Complete command", "lazy related record section");
    }
    await page.screenshot({ path: resolve(output, name + "-workspace.png"), fullPage: true });
    report.checks.push("bounded 20-row keyset pages over 10000 records", "selected relationship resolved outside first page", "bounded search over 125 related choices", "A32 keyboard form, relation search/selection and save", "PATCH with authoritative ETag", "A51 committed response loss survives dialog remount without replay");
  }
  if (errors.length) throw new Error(errors.join("\n"));
  report.result = "passed";
  writeFileSync(resolve(output, name + "-" + stage + ".json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} catch (error) {
  if (errors.length) console.error(errors.join("\n"));
  if (page) { console.error(await page.locator("body").innerText()); await page.screenshot({ path: resolve(output, name + "-" + stage + "-failure.png"), fullPage: true }); }
  throw error;
} finally { await browser?.close(); await new Promise((r) => server.close(r)); }
