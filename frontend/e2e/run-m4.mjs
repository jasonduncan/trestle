#!/usr/bin/env node
// M4 end-to-end demonstration in a real browser (design doc §18 M4).
//
//   1. Product (generated SDK + runtime form): a teacher adds a unit and
//      creates lessons, picking the unit from a scoped relation picker that
//      never shows another school's units.
//   2. The same publish operation from two clients: the product screen
//      (teacher) and the admin shell (owner) both call
//      POST /api/v1/lessons/{id}/actions/publish; the audit log shows
//      lessons.publish for both actors.
//   3. Admin edit with a stale ETag gets the conflict message; after reload
//      the edit sends If-Match and only the changed field.
//   4. A teacher cannot open the admin profile.
//
// Writes docs/evidence/m4-demo.json and screenshots. Every process this
// script starts is tracked and stopped by its own PID.

import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdirSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const evidence = join(repo, "docs/evidence");
const API_PORT = 8032;
const PRODUCT_PORT = 4175;
const ADMIN_PORT = 4176;
const API = `http://127.0.0.1:${API_PORT}`;
const NORTH = "11111111-1111-4111-8111-111111111111";
const SOUTH = "22222222-2222-4222-8222-222222222222";
const DEV_TOKENS = [
  "alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
  "carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc",
  "olivia-dev-token=dddddddd-dddd-4ddd-8ddd-dddddddddddd",
].join(",");

const children = new Set();
const report = { demonstration: "M4: resources, generated client and admin on the same operations", startedAt: new Date().toISOString(), steps: [] };
const log = (m) => console.log(`[m4-demo] ${m}`);
const sh = (cmd, args, options = {}) => execFileSync(cmd, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], ...options }).trim();
function check(condition, message) {
  if (!condition) throw new Error(`check failed: ${message}`);
}
function step(name, details) {
  report.steps.push({ name, ...details });
  log(`✓ ${name}`);
}

async function portFree(port) {
  const server = createServer();
  try {
    server.listen(port, "127.0.0.1");
    await once(server, "listening");
    return true;
  } catch {
    return false;
  } finally {
    server.close();
  }
}

function start(name, cmd, args, { env = {}, cwd = tmpdir() } = {}) {
  const child = spawn(cmd, args, { cwd, env: { ...process.env, ...env }, stdio: ["ignore", "ignore", "pipe"] });
  child.name = name;
  child.errors = "";
  child.stderr.on("data", (chunk) => (child.errors += chunk));
  children.add(child);
  log(`started ${name} (pid ${child.pid})`);
  return child;
}

async function stop(child) {
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit");
    child.kill("SIGTERM"); // this child's own PID only
    await exited;
  }
  children.delete(child);
  log(`stopped ${child.name} (pid ${child.pid})`);
}

async function waitFor(url, child) {
  for (let i = 0; i < 150; i++) {
    if (child.exitCode !== null) throw new Error(`${child.name} exited early:\n${child.errors}`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out waiting for ${url}`);
}

const vite = join(repo, "frontend/node_modules/vite/bin/vite.js");

/** Clicks and returns the matching API request/response. */
async function clickFor(page, locator, method, pathPattern) {
  const [response] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === method && pathPattern.test(new URL(r.url()).pathname)),
    locator.click(),
  ]);
  return {
    path: new URL(response.url()).pathname,
    status: response.status(),
    requestHeaders: response.request().headers(),
    payload: response.request().postDataJSON?.() ?? null,
    body: await response.json().catch(() => null),
  };
}

async function signIn(page, user, school = NORTH) {
  await page.getByLabel("User").selectOption(user);
  await page.getByLabel("School").selectOption(school);
}

async function main() {
  for (const port of [API_PORT, PRODUCT_PORT, ADMIN_PORT]) check(await portFree(port), `port ${port} is free`);
  mkdirSync(evidence, { recursive: true });
  const commit = sh("git", ["-C", repo, "rev-parse", "HEAD"]);
  const dirty = sh("git", ["-C", repo, "status", "--porcelain", "--", ".", ":!docs/evidence"]).length > 0;
  report.release = { commit, uncommittedChanges: dirty };

  log("building API and both bundles …");
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: repo });
  for (const app of ["product", "admin"]) sh(process.execPath, [vite, "build", "--logLevel", "error"], { cwd: join(repo, "frontend", app) });
  const bin = join(repo, "target/debug");

  const db = `trestle_e2e_${Date.now()}`;
  const url = `postgres://localhost/${db}`;
  const psql = (sql) => sh("psql", ["-d", db, "-qAtc", sql]);
  sh("createdb", [db]);
  const env = { DATABASE_URL: url, TRESTLE_ENV: "development", TRESTLE_BIND: `127.0.0.1:${API_PORT}`, TRESTLE_DEV_TOKENS: DEV_TOKENS, RUST_LOG: "warn" };
  let browser;
  try {
    sh(join(bin, "pilot-migrate"), [], { env: { ...process.env, DATABASE_URL: url }, cwd: tmpdir() });
    sh(join(bin, "pilot"), ["seed-dev"], { env: { ...process.env, ...env }, cwd: tmpdir() });
    const api = start("api", join(bin, "pilot"), ["serve"], { env });
    await waitFor(`${API}/readyz`, api);
    const preview = (app, port) =>
      start(`${app} preview`, process.execPath, [vite, "preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"], {
        cwd: join(repo, "frontend", app),
        env: { TRESTLE_API: API },
      });
    const product = preview("product", PRODUCT_PORT);
    const admin = preview("admin", ADMIN_PORT);
    await waitFor(`http://127.0.0.1:${PRODUCT_PORT}`, product);
    await waitFor(`http://127.0.0.1:${ADMIN_PORT}/admin/`, admin);

    browser = await chromium.launch({ channel: "chrome", headless: true });
    const shop = await browser.newPage();
    await shop.goto(`http://127.0.0.1:${PRODUCT_PORT}`);

    // 1. Units via the generated client, lessons via the runtime form.
    await signIn(shop, "bob", SOUTH);
    await shop.getByLabel("New unit").fill("Southside unit");
    const bobUnit = await clickFor(shop, shop.getByRole("button", { name: "Add unit" }), "POST", /^\/api\/v1\/units$/);
    check(bobUnit.status === 201, "bob adds a unit in his school");
    await signIn(shop, "alice");
    await shop.getByLabel("New unit").fill("Number sense");
    const unit = await clickFor(shop, shop.getByRole("button", { name: "Add unit" }), "POST", /^\/api\/v1\/units$/);
    check(unit.status === 201, "alice adds a unit");
    const picker = shop.getByLabel("Unit (optional)");
    await picker.locator("option", { hasText: "Number sense" }).waitFor({ state: "attached" });
    const offered = await picker.locator("option").allInnerTexts();
    check(!offered.includes("Southside unit"), "the relation picker never offers another school's unit");
    await shop.getByLabel("Title", { exact: true }).fill("Fractions");
    await shop.getByLabel("Grade level", { exact: true }).fill("4");
    await picker.selectOption({ label: "Number sense" });
    const fractions = await clickFor(shop, shop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(fractions.status === 201 && fractions.body.unit_id === unit.body.id, "lesson created in the chosen unit");
    await shop.getByLabel("Title", { exact: true }).fill("Ratios");
    await shop.getByLabel("Grade level", { exact: true }).fill("6");
    const ratios = await clickFor(shop, shop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(ratios.status === 201, "second lesson created");
    step("product: scoped relation picker and generated client", {
      offeredUnits: offered,
      created: [fractions.payload, ratios.payload],
    });

    // 2. The same publish operation from product and admin.
    const productPublish = await clickFor(shop, shop.getByRole("button", { name: "Publish Fractions" }), "POST", /\/actions\/publish$/);
    check(productPublish.status === 200 && productPublish.body.status === "published", "product publish");
    await shop.getByText("Published “Fractions”.").waitFor();
    await shop.screenshot({ path: join(evidence, "m4-product.png"), fullPage: true });

    const desk = await browser.newPage();
    await desk.goto(`http://127.0.0.1:${ADMIN_PORT}/admin/`);
    await desk.getByRole("heading", { name: "Lessons" }).waitFor();
    const adminPublish = await clickFor(desk, desk.getByRole("button", { name: "Publish Ratios" }), "POST", /\/actions\/publish$/);
    check(adminPublish.status === 200 && adminPublish.body.status === "published", "admin publish");
    await desk.getByText("Publish: “Ratios” done.").waitFor();
    check((await desk.getByRole("button", { name: /^Publish / }).count()) === 0, "published rows offer no Publish button");
    const template = (p) => p.replace(/[0-9a-f-]{36}/, "{id}");
    check(template(productPublish.path) === template(adminPublish.path), "both clients call the same route");
    const audit = psql("SELECT actor_id || ' ' || operation FROM audit_events WHERE operation = 'lessons.publish' ORDER BY occurred_at")
      .split("\n")
      .filter(Boolean);
    check(
      audit[0] === "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa lessons.publish" && audit[1] === "dddddddd-dddd-4ddd-8ddd-dddddddddddd lessons.publish",
      `audit shows both actors: ${audit}`,
    );
    step("product and admin invoke the same publish operation", {
      product: { path: productPublish.path, payload: productPublish.payload, status: productPublish.status },
      admin: { path: adminPublish.path, payload: adminPublish.payload, status: adminPublish.status },
      audit,
    });

    // 3. Admin edit: stale ETag, then a clean edit sending only the change.
    const opened = await clickFor(desk, desk.getByRole("button", { name: "Edit Fractions" }), "GET", /^\/api\/v1\/lessons\/[0-9a-f-]{36}$/);
    await desk.getByLabel("Subtitle (optional)").waitFor();
    // Someone else changes the lesson meanwhile (through the API).
    const bump = await fetch(`${API}/api/v1/lessons/${fractions.body.id}`, {
      method: "PATCH",
      headers: {
        Authorization: "Bearer alice-dev-token",
        "Trestle-Tenant": NORTH,
        "Content-Type": "application/json",
        "If-Match": opened.body && `"lessons.view:${opened.body.version}"`,
      },
      body: JSON.stringify({ grade_level: 5 }),
    });
    check(bump.status === 200, "concurrent edit");
    await desk.getByLabel("Subtitle (optional)").fill("Parts of a whole");
    const stale = await clickFor(desk, desk.getByRole("button", { name: "Save changes" }), "PATCH", /^\/api\/v1\/lessons\//);
    check(stale.status === 412, `stale admin edit refused (got ${stale.status})`);
    await desk.getByText("Someone changed this since you opened it").waitFor();
    await desk.getByRole("button", { name: "Close" }).click();
    await clickFor(desk, desk.getByRole("button", { name: "Edit Fractions" }), "GET", /^\/api\/v1\/lessons\/[0-9a-f-]{36}$/);
    await desk.getByLabel("Subtitle (optional)").fill("Parts of a whole");
    const saved = await clickFor(desk, desk.getByRole("button", { name: "Save changes" }), "PATCH", /^\/api\/v1\/lessons\//);
    check(saved.status === 200 && saved.body.subtitle === "Parts of a whole" && saved.body.grade_level === 5, "fresh edit keeps the other change");
    check(JSON.stringify(Object.keys(saved.payload)) === JSON.stringify(["subtitle"]), `only the changed field is sent: ${JSON.stringify(saved.payload)}`);
    await desk.screenshot({ path: join(evidence, "m4-admin.png"), fullPage: true });
    step("admin edit: stale ETag refused, then only the change is sent", {
      staleIfMatch: stale.requestHeaders["if-match"],
      staleStatus: stale.status,
      freshIfMatch: saved.requestHeaders["if-match"],
      freshPayload: saved.payload,
      result: saved.body,
    });

    // 4. The admin profile is owner-only.
    await signIn(desk, "alice");
    await desk.getByText("Only school owners can use the admin.").waitFor();
    step("teachers cannot open the admin profile", { message: "Only school owners can use the admin." });

    report.result = "passed";
  } catch (error) {
    report.result = "failed";
    report.error = String(error?.stack ?? error);
    throw error;
  } finally {
    report.finishedAt = new Date().toISOString();
    writeFileSync(join(evidence, "m4-demo.json"), `${JSON.stringify(report, null, 2)}\n`);
    await browser?.close();
    for (const child of [...children]) await stop(child);
    try {
      sh("dropdb", ["--if-exists", "--force", db]);
    } catch (error) {
      log(`could not drop ${db}: ${error}`);
    }
  }
}

main().then(
  () => log("all checks passed; report in docs/evidence/m4-demo.json"),
  (error) => {
    console.error(error);
    process.exitCode = 1;
  },
);
