#!/usr/bin/env node
// M5 end-to-end demonstration in a real browser (design doc §18 M5 "Show").
//
// Rolling deploy of an additive change with old clients still open:
//   1. The M4 release (API binary + browser bundles built from the tag
//      m4-complete) serves traffic; an owner opens an edit form (in flight).
//   2. The expand migration (lessons.0004_duration) is applied while the
//      old replica serves; old writes keep working.
//   3. A new replica joins behind an edge that can route to either
//      replica. The in-flight M4 edit is submitted to the new replica; a
//      new bundle writes the new field; a new-revision submission that
//      lands on the old replica is refused before any write and the form
//      reloads (409 interaction_stale); mixed round-robin traffic works.
//   4. A stale in-flight edit is refused (412) and, reopened, leaves the new
//      field untouched. The old replica is retired; old bundles keep working.
//   5. Production identity profile: production boot refuses unsafe
//      configuration; a teacher signs in through an OpenID Connect provider
//      (mock) into a server-side session; a same-site page on another origin
//      cannot write with that cookie (origin + CSRF checks); sign-out
//      revokes the session.
//
// Writes docs/evidence/m5-demo.json and screenshots. Every process this
// script starts is tracked and stopped by its own PID.

import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer as createHttpServer, request as httpRequest } from "node:http";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

import { startMockIdp } from "./mock-idp.mjs";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const evidence = join(repo, "docs/evidence");
const OLD_TAG = process.env.OLD_TAG ?? "m4-complete";
const PORTS = {
  oldApi: 18101,
  newApi: 18102,
  sessionApi: 18103,
  oldProduct: 18110,
  oldAdmin: 18111,
  newProduct: 18120,
  sessionProduct: 18130,
  idp: 18140,
  evil: 18150,
};
const NORTH = "11111111-1111-4111-8111-111111111111";
const DEV_TOKENS = [
  "alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
  "carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc",
  "olivia-dev-token=dddddddd-dddd-4ddd-8ddd-dddddddddddd",
].join(",");

const children = new Set();
const servers = [];
const report = {
  demonstration: "M5: rolling additive change with old clients in flight, and the production identity profile",
  startedAt: new Date().toISOString(),
  steps: [],
};
const log = (m) => console.log(`[m5-demo] ${m}`);
const sh = (cmd, args, options = {}) =>
  execFileSync(cmd, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], maxBuffer: 64 * 1024 * 1024, ...options }).trim();
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
  (report.stopped ??= []).push({ process: child.name, exit: child.exitCode ?? child.signalCode });
  log(`stopped ${child.name} (pid ${child.pid}, exit ${child.exitCode ?? child.signalCode})`);
}

async function waitFor(url, child) {
  for (let i = 0; i < 300; i++) {
    if (child && child.exitCode !== null) throw new Error(`${child.name} exited early:\n${child.errors}`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out waiting for ${url}`);
}

/** sha256 over every file in a bundle directory (path + content). */
function digestDir(dir) {
  const hash = createHash("sha256");
  const walk = (d) => {
    for (const name of readdirSync(d).sort()) {
      const path = join(d, name);
      if (statSync(path).isDirectory()) walk(path);
      else hash.update(path.slice(dir.length)).update(readFileSync(path));
    }
  };
  walk(dir);
  return `sha256:${hash.digest("hex")}`;
}

// The edge ------------------------------------------------------------------
// One public origin per bundle: static files from the bundle, /api and
// /_meta forwarded to the replica pool. The pool's routing is switchable so
// each compatibility case is deterministic; every forwarded request is
// logged with the replica that served it.

const pool = { upstreams: {}, mode: "old", next: 0, log: [] };

function pick() {
  const names = Object.keys(pool.upstreams);
  if (pool.mode === "round-robin") return names[pool.next++ % names.length];
  return pool.mode;
}

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".json": "application/json" };

async function startEdge(name, port, dist, route = pick) {
  const server = createHttpServer(async (req, res) => {
    const url = new URL(req.url, "http://edge");
    if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/_meta/")) {
      const replica = route();
      const target = pool.upstreams[replica];
      const upstream = httpRequest(
        { host: "127.0.0.1", port: target, method: req.method, path: req.url, headers: { ...req.headers, "x-real-ip": "127.0.0.1" } },
        async (up) => {
          const chunks = [];
          for await (const chunk of up) chunks.push(chunk);
          const body = Buffer.concat(chunks);
          let code;
          if (up.statusCode >= 400) {
            try {
              code = JSON.parse(body.toString()).code;
            } catch {}
          }
          pool.log.push({
            edge: name,
            method: req.method,
            path: url.pathname,
            servedBy: replica,
            status: up.statusCode,
            code,
            origin: req.headers.origin,
            sessionCookie: /__Host-trestle_session=/.test(req.headers.cookie ?? ""),
            revision: req.headers["trestle-interaction-revision"],
          });
          res.writeHead(up.statusCode, { ...up.headers, "x-served-by": replica });
          res.end(body);
        },
      );
      upstream.on("error", (error) => {
        pool.log.push({ edge: name, method: req.method, path: url.pathname, servedBy: replica, error: error.code });
        res.writeHead(502, { "Content-Type": "application/problem+json" });
        res.end(JSON.stringify({ status: 502, code: "bad_gateway" }));
      });
      req.pipe(upstream);
      return;
    }
    // Static bundle. Unknown non-API paths get the entry document; the edge
    // never rewrites /api or /_meta (§16.2).
    let file = normalize(join(dist, decodeURIComponent(url.pathname)));
    if (!file.startsWith(dist) || !existsSync(file) || statSync(file).isDirectory()) file = join(dist, "index.html");
    res.writeHead(200, { "Content-Type": TYPES[extname(file)] ?? "application/octet-stream", "Cache-Control": "no-store" });
    res.end(readFileSync(file));
  });
  server.listen(port, "127.0.0.1");
  await once(server, "listening");
  servers.push(server);
  return server;
}

async function startEvilSite(port, target) {
  // A page on another origin of the same site (localhost). It cannot read
  // the CSRF token (no CORS), so it tries a "simple" cross-origin write.
  const html = `<!doctype html><title>Other site</title><h1>A page on another origin</h1>
<script>
fetch(${JSON.stringify(`${target}/api/v1/lessons`)}, {
  method: "POST", mode: "no-cors", credentials: "include",
  headers: { "Content-Type": "text/plain" },
  body: JSON.stringify({ title: "Forged by another origin", grade_level: 3 }),
}).finally(() => { document.title = "sent"; });
</script>`;
  const server = createHttpServer((req, res) => {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end(html);
  });
  server.listen(port, "127.0.0.1");
  await once(server, "listening");
  servers.push(server);
}

/** Clicks and returns the matching API request/response. */
async function clickFor(page, locator, method, pathPattern) {
  const [response] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === method && pathPattern.test(new URL(r.url()).pathname)),
    locator.click(),
  ]);
  return {
    path: new URL(response.url()).pathname,
    status: response.status(),
    servedBy: response.headers()["x-served-by"],
    requestHeaders: await response.request().allHeaders(),
    payload: response.request().postDataJSON?.() ?? null,
    body: await response.json().catch(() => null),
  };
}

// Builds ----------------------------------------------------------------------

function buildOldRelease() {
  const source = join(repo, "target/m4-source");
  const targetDir = join(repo, "target/m4-release");
  rmSync(source, { recursive: true, force: true });
  mkdirSync(source, { recursive: true });
  // The tagged tree, without touching the working copy or git metadata.
  execFileSync("sh", ["-c", `git -C "${repo}" archive ${OLD_TAG} | tar -x -C "${source}"`]);
  log(`building ${OLD_TAG} API (separate target directory) …`);
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: source, env: { ...process.env, CARGO_TARGET_DIR: targetDir } });
  log(`building ${OLD_TAG} bundles …`);
  sh("npm", ["ci", "--prefer-offline", "--no-audit", "--no-fund", "--loglevel", "error"], { cwd: join(source, "frontend") });
  const vite = join(source, "frontend/node_modules/vite/bin/vite.js");
  for (const app of ["product", "admin"]) sh(process.execPath, [vite, "build", "--logLevel", "error"], { cwd: join(source, "frontend", app) });
  return {
    bin: join(targetDir, "debug"),
    product: join(source, "frontend/product/dist"),
    admin: join(source, "frontend/admin/dist"),
    commit: sh("git", ["-C", repo, "rev-list", "-n", "1", OLD_TAG]),
  };
}

function buildNewRelease() {
  log("building the current API and bundles …");
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: repo });
  const vite = join(repo, "frontend/node_modules/vite/bin/vite.js");
  for (const app of ["product", "admin"]) sh(process.execPath, [vite, "build", "--logLevel", "error"], { cwd: join(repo, "frontend", app) });
  return {
    bin: join(repo, "target/debug"),
    product: join(repo, "frontend/product/dist"),
    admin: join(repo, "frontend/admin/dist"),
    commit: sh("git", ["-C", repo, "rev-parse", "HEAD"]),
  };
}

// The demonstration ---------------------------------------------------------

async function main() {
  for (const port of Object.values(PORTS)) check(await portFree(port), `port ${port} is free`);
  mkdirSync(evidence, { recursive: true });
  const dirty = sh("git", ["-C", repo, "status", "--porcelain", "--", ".", ":!docs/evidence"]).length > 0;

  const old = buildOldRelease();
  const cur = buildNewRelease();
  report.releases = {
    old: { tag: OLD_TAG, commit: old.commit, productBundle: digestDir(old.product), adminBundle: digestDir(old.admin) },
    new: { commit: cur.commit, uncommittedChanges: dirty, productBundle: digestDir(cur.product), adminBundle: digestDir(cur.admin) },
  };

  const db = `trestle_m5_${Date.now()}`;
  const url = `postgres://localhost/${db}`;
  const psql = (sql) => sh("psql", ["-d", db, "-qAtc", sql]);
  const dbEnv = { DATABASE_URL: url, RUST_LOG: "warn" };
  const apiEnv = (port) => ({ ...dbEnv, TRESTLE_ENV: "development", TRESTLE_BIND: `127.0.0.1:${port}`, TRESTLE_DEV_TOKENS: DEV_TOKENS });
  sh("createdb", [db]);
  let browser;
  try {
    // 1. The M4 world. -------------------------------------------------------
    const oldMigrate = sh(join(old.bin, "pilot-migrate"), [], { env: { ...process.env, ...dbEnv }, cwd: tmpdir() });
    sh(join(old.bin, "pilot"), ["seed-dev"], { env: { ...process.env, ...apiEnv(PORTS.oldApi) }, cwd: tmpdir() });
    const oldApi = start("old replica (M4)", join(old.bin, "pilot"), ["serve"], { env: apiEnv(PORTS.oldApi) });
    await waitFor(`http://127.0.0.1:${PORTS.oldApi}/readyz`, oldApi);
    pool.upstreams.old = PORTS.oldApi;
    pool.mode = "old";
    await startEdge("old product", PORTS.oldProduct, old.product);
    await startEdge("old admin", PORTS.oldAdmin, old.admin);
    await startEdge("new product", PORTS.newProduct, cur.product);

    browser = await chromium.launch({ channel: "chrome", headless: true });
    const oldShop = await browser.newPage();
    await oldShop.goto(`http://127.0.0.1:${PORTS.oldProduct}`);
    await oldShop.getByLabel("Title", { exact: true }).fill("Fractions");
    await oldShop.getByLabel("Grade level", { exact: true }).fill("4");
    const fractions = await clickFor(oldShop, oldShop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(fractions.status === 201 && fractions.servedBy === "old", "M4 bundle creates through the M4 replica");
    check(!("duration_minutes" in fractions.body), "the M4 API has no duration field");

    const oldDesk = await browser.newPage();
    await oldDesk.goto(`http://127.0.0.1:${PORTS.oldAdmin}`);
    await oldDesk.getByRole("heading", { name: "Lessons" }).waitFor();
    const opened = await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Edit Fractions" }), "GET", /^\/api\/v1\/lessons\/[0-9a-f-]{36}$/);
    await oldDesk.getByLabel(/^Title/).fill("Fractions (edited during the deploy)");
    step("M4 release serving; an owner opens an edit form and starts typing", {
      migrations: oldMigrate.split("\n"),
      created: { status: fractions.status, servedBy: fractions.servedBy, body: fractions.body },
      inFlightEdit: { opened: opened.body?.title, etag: `"lessons.view:${opened.body?.version}"` },
    });

    // 2. Expand migration while the old replica serves. -----------------------
    const plan = sh(join(cur.bin, "pilot-migrate"), ["--plan"], { env: { ...process.env, ...dbEnv }, cwd: tmpdir() });
    const expand = sh(join(cur.bin, "pilot-migrate"), [], { env: { ...process.env, ...dbEnv }, cwd: tmpdir() });
    check(/lessons\.0004_duration/.test(expand), "expand migration applied");
    await oldShop.getByLabel("Title", { exact: true }).fill("Decimals");
    await oldShop.getByLabel("Grade level", { exact: true }).fill("5");
    const decimals = await clickFor(oldShop, oldShop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(decimals.status === 201 && decimals.servedBy === "old", "old replica keeps writing after the expand migration");
    const oldReady = await fetch(`http://127.0.0.1:${PORTS.oldApi}/readyz`);
    check(oldReady.ok, "old replica still ready");
    step("expand migration applied while the M4 replica serves", {
      plan: plan.split("\n"),
      applied: expand.split("\n"),
      oldReplicaWriteAfterExpand: { status: decimals.status, servedBy: decimals.servedBy },
      column: psql("SELECT column_name || ' ' || data_type || ' nullable=' || is_nullable FROM information_schema.columns WHERE table_name = 'lessons' AND column_name = 'duration_minutes'"),
    });

    // 3. New replica joins. ---------------------------------------------------
    const newApi = start("new replica", join(cur.bin, "pilot"), ["serve"], { env: apiEnv(PORTS.newApi) });
    await waitFor(`http://127.0.0.1:${PORTS.newApi}/readyz`, newApi);
    pool.upstreams.new = PORTS.newApi;

    // The in-flight M4 edit lands on the new replica.
    pool.mode = "new";
    const inFlight = await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Save changes" }), "PATCH", /^\/api\/v1\/lessons\//);
    check(inFlight.status === 200 && inFlight.servedBy === "new", `in-flight M4 edit accepted by the new replica (got ${inFlight.status})`);
    check(inFlight.body.title === "Fractions (edited during the deploy)" && inFlight.body.duration_minutes === null, "edit applied; new field untouched");
    check(inFlight.requestHeaders["trestle-interaction-revision"] === "r-9adfca22d27dfc4e", "submitted with the M4 admin edit revision");
    await oldDesk.screenshot({ path: join(evidence, "m5-inflight.png"), fullPage: true });

    // A new bundle writes the new field.
    const shop = await browser.newPage();
    await shop.goto(`http://127.0.0.1:${PORTS.newProduct}`);
    await shop.getByLabel("Duration (minutes) (optional)").waitFor();
    await shop.getByLabel("Title", { exact: true }).fill("Geometry");
    await shop.getByLabel("Grade level", { exact: true }).fill("6");
    await shop.getByLabel("Duration (minutes) (optional)").fill("45");
    const geometry = await clickFor(shop, shop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(geometry.status === 201 && geometry.servedBy === "new" && geometry.body.duration_minutes === 45, "new bundle writes duration through the new replica");

    // Skew: a new-revision submission reaches the old replica.
    pool.mode = "old";
    const before = Number(psql("SELECT count(*) FROM lessons"));
    await shop.getByLabel("Title", { exact: true }).fill("Measurement");
    await shop.getByLabel("Grade level", { exact: true }).fill("3");
    await shop.getByLabel("Duration (minutes) (optional)").fill("30");
    const skewed = await clickFor(shop, shop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(skewed.status === 409 && skewed.body.code === "interaction_stale" && skewed.servedBy === "old", "old replica refuses the unknown revision");
    await shop.getByText("This form changed since you opened it").waitFor();
    check(Number(psql("SELECT count(*) FROM lessons")) === before, "nothing was written");
    const durationShown = await shop.getByLabel("Duration (minutes) (optional)").count();
    const keptTitle = await shop.getByLabel("Title", { exact: true }).inputValue();
    const retried = await clickFor(shop, shop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(retried.status === 201, "after review the reloaded form submits");
    step("mixed versions: in-flight edit, new field, and skew handled before any write", {
      inFlightEdit: {
        servedBy: inFlight.servedBy,
        status: inFlight.status,
        revision: inFlight.requestHeaders["trestle-interaction-revision"],
        ifMatch: inFlight.requestHeaders["if-match"],
        payload: inFlight.payload,
        result: inFlight.body,
      },
      newBundleWrite: { servedBy: geometry.servedBy, status: geometry.status, payload: geometry.payload },
      skew: {
        servedBy: skewed.servedBy,
        status: skewed.status,
        code: skewed.body.code,
        revision: skewed.requestHeaders["trestle-interaction-revision"],
        lessonsWritten: 0,
        formAfterReload: { durationFieldShown: durationShown > 0, titleKept: keptTitle },
        retry: { servedBy: retried.servedBy, status: retried.status, payload: retried.payload },
      },
    });

    // Round-robin: both bundles against both replicas.
    pool.mode = "round-robin";
    const mark = pool.log.length;
    for (let i = 0; i < 3; i++) {
      await shop.reload();
      await shop.getByRole("heading", { name: "Lessons" }).waitFor();
      await oldShop.reload();
      await oldShop.getByRole("heading", { name: "Lessons" }).waitFor();
    }
    await shop.waitForTimeout(500);
    const mixed = pool.log.slice(mark).filter((e) => e.path !== "/api/v1/auth/session");
    const servedBy = mixed.reduce((acc, e) => ({ ...acc, [e.servedBy]: (acc[e.servedBy] ?? 0) + 1 }), {});
    const failures = mixed.filter((e) => e.status >= 400);
    check(servedBy.old > 0 && servedBy.new > 0 && failures.length === 0, `round-robin reads all succeed: ${JSON.stringify({ servedBy, failures })}`);
    step("round-robin reads from old and new bundles across both replicas", { requests: mixed.length, servedBy, failures: failures.length });

    // 4. A stale in-flight edit from an M4 form, then retirement. -----------
    // The M4 admin (re)loads while the old replica still serves, so its edit
    // form is the M4 revision, which knows nothing of duration_minutes.
    pool.mode = "old";
    await oldDesk.reload();
    await oldDesk.getByRole("heading", { name: "Lessons" }).waitFor();
    const reopened = await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Edit Geometry" }), "GET", /^\/api\/v1\/lessons\/[0-9a-f-]{36}$/);
    await oldDesk.getByLabel(/^Grade level/).waitFor();
    check((await oldDesk.getByLabel(/^Duration/).count()) === 0, "the M4 edit form has no duration field");
    const elsewhere = await fetch(`http://127.0.0.1:${PORTS.newApi}/api/v1/lessons/${geometry.body.id}`, {
      method: "PATCH",
      headers: {
        Authorization: "Bearer alice-dev-token",
        "Trestle-Tenant": NORTH,
        "Content-Type": "application/json",
        "If-Match": `"lessons.view:${reopened.body.version}"`,
      },
      body: JSON.stringify({ title: "Geometry (renamed elsewhere)" }),
    });
    check(elsewhere.status === 200, "a concurrent edit elsewhere");
    pool.mode = "new";
    await oldDesk.getByLabel(/^Grade level/).fill("7");
    const stale = await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Save changes" }), "PATCH", /^\/api\/v1\/lessons\//);
    check(stale.status === 412 && stale.servedBy === "new", `stale in-flight edit refused (got ${stale.status})`);
    await oldDesk.getByText("Someone changed this since you opened it").waitFor();
    await oldDesk.getByRole("button", { name: "Close" }).click();
    pool.mode = "old";
    await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Edit Geometry" }), "GET", /^\/api\/v1\/lessons\/[0-9a-f-]{36}$/);
    await oldDesk.getByLabel(/^Grade level/).fill("7");
    pool.mode = "new";
    const fresh = await clickFor(oldDesk, oldDesk.getByRole("button", { name: "Save changes" }), "PATCH", /^\/api\/v1\/lessons\//);
    check(fresh.status === 200 && fresh.servedBy === "new", `reopened M4 edit accepted by the new replica (got ${fresh.status})`);
    check(fresh.body.grade_level === 7 && fresh.body.duration_minutes === 45 && fresh.body.title === "Geometry (renamed elsewhere)", "the M4 edit keeps both the other change and the new field");
    check(JSON.stringify(Object.keys(fresh.payload)) === JSON.stringify(["grade_level"]), `M4 client sends only its change: ${JSON.stringify(fresh.payload)}`);
    check(fresh.requestHeaders["trestle-interaction-revision"] === "r-9adfca22d27dfc4e", "submitted with the M4 admin edit revision");
    step("stale M4 edit refused; reopened M4 edit leaves the new field alone", {
      stale: { servedBy: stale.servedBy, status: stale.status, ifMatch: stale.requestHeaders["if-match"], revision: stale.requestHeaders["trestle-interaction-revision"] },
      fresh: {
        servedBy: fresh.servedBy,
        status: fresh.status,
        ifMatch: fresh.requestHeaders["if-match"],
        revision: fresh.requestHeaders["trestle-interaction-revision"],
        payload: fresh.payload,
      },
      result: fresh.body,
    });

    await stop(oldApi);
    delete pool.upstreams.old;
    await oldShop.reload();
    await oldShop.getByLabel("Title", { exact: true }).fill("Ratios");
    await oldShop.getByLabel("Grade level", { exact: true }).fill("6");
    const ratios = await clickFor(oldShop, oldShop.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(ratios.status === 201 && ratios.servedBy === "new", "M4 bundle keeps working after the old replica is gone");
    await shop.reload();
    await shop.getByRole("cell", { name: "Ratios", exact: true }).waitFor();
    await shop.screenshot({ path: join(evidence, "m5-rollout.png"), fullPage: true });
    const lessons = psql("SELECT title || ' | ' || coalesce(duration_minutes::text, '-') || ' | v' || version FROM lessons ORDER BY created_at").split("\n");
    step("old replica retired; the M4 bundle still works against the new replica", {
      oldBundleWrite: { servedBy: ratios.servedBy, status: ratios.status, payload: ratios.payload },
      lessons,
    });

    // 5. Production identity profile. ----------------------------------------
    const refused = (() => {
      try {
        sh(join(cur.bin, "pilot"), ["serve"], {
          env: { ...process.env, ...dbEnv, TRESTLE_ENV: "production", TRESTLE_BIND: `127.0.0.1:${PORTS.sessionApi}`, TRESTLE_DEV_TOKENS: DEV_TOKENS },
          cwd: tmpdir(),
        });
        return { exitCode: 0 };
      } catch (error) {
        return { exitCode: error.status, stderr: String(error.stderr).trim().split("\n") };
      }
    })();
    check(refused.exitCode !== 0 && refused.stderr.some((l) => l.includes("TRESTLE_DEV_TOKENS must not be set")), "production refuses dev tokens");

    const clientSecret = "demo-client-secret-not-a-real-one";
    const idp = await startMockIdp({ port: PORTS.idp, clientId: "pilot-web", clientSecret, subject: "e2e-teacher", name: "E2E Teacher" });
    servers.push({ close: idp.close });
    const cli = (args) => sh(join(cur.bin, "pilot"), args, { env: { ...process.env, ...dbEnv }, cwd: tmpdir() });
    const actor = cli(["identity", "actor-id", "--issuer", idp.issuer, "--subject", "e2e-teacher"]);
    const provisioned = cli(["provision", "member", "--school", NORTH, "--actor", actor, "--role", "teacher"]);
    const origin = `http://localhost:${PORTS.sessionProduct}`;
    const sessionApi = start("session replica", join(cur.bin, "pilot"), ["serve"], {
      env: {
        ...dbEnv,
        TRESTLE_ENV: "development",
        TRESTLE_BIND: `127.0.0.1:${PORTS.sessionApi}`,
        TRESTLE_OIDC_ISSUER: idp.issuer,
        TRESTLE_OIDC_CLIENT_ID: "pilot-web",
        TRESTLE_OIDC_CLIENT_SECRET: clientSecret,
        TRESTLE_PUBLIC_ORIGIN: origin,
        TRESTLE_SESSION_KEY: "demo session key, per run, at least 32 bytes long",
        TRESTLE_CURSOR_KEY: "demo cursor key, per run, at least 32 bytes long",
        TRESTLE_CLIENT_ADDRESS_HEADER: "x-real-ip",
      },
    });
    await waitFor(`http://127.0.0.1:${PORTS.sessionApi}/readyz`, sessionApi);
    pool.upstreams.session = PORTS.sessionApi;
    await startEdge("session product", PORTS.sessionProduct, cur.product, () => "session");
    await startEvilSite(PORTS.evil, origin);

    const context = await browser.newContext();
    const app = await context.newPage();
    await app.goto(origin);
    await app.getByRole("heading", { name: "Sign in" }).waitFor();
    await app.getByRole("link", { name: "Sign in with your organization" }).click();
    await app.getByRole("heading", { name: "Mock identity provider" }).waitFor();
    const authorize = idp.events.find((e) => e.event === "authorize").params;
    await app.screenshot({ path: join(evidence, "m5-idp.png") });
    await app.getByRole("link", { name: "Continue" }).click();
    await app.getByText("Signed in").waitFor();
    await app.getByLabel("School").locator("option", { hasText: "Northside Elementary (teacher)" }).waitFor({ state: "attached" });
    const cookies = (await context.cookies(origin)).map(({ name, httpOnly, secure, sameSite, path, domain }) => ({ name, httpOnly, secure, sameSite, path, domain }));
    const session = cookies.find((c) => c.name === "__Host-trestle_session");
    check(session && session.httpOnly && session.secure && session.sameSite === "Strict" && session.path === "/", `session cookie attributes: ${JSON.stringify(cookies)}`);
    check(!cookies.some((c) => c.name === "__Host-trestle_login"), "login cookie cleared");

    await app.getByLabel("Title", { exact: true }).fill("Signed-in lesson");
    await app.getByLabel("Grade level", { exact: true }).fill("2");
    await app.getByLabel("Duration (minutes) (optional)").fill("20");
    const signedWrite = await clickFor(app, app.getByRole("button", { name: "Create lesson" }), "POST", /^\/api\/v1\/lessons$/);
    check(signedWrite.status === 201, `signed-in write (got ${signedWrite.status} ${JSON.stringify(signedWrite.body)})`);
    check(Boolean(signedWrite.requestHeaders["x-csrf-token"]) && !signedWrite.requestHeaders.authorization, "session write carries a CSRF token and no bearer token");
    const createdBy = psql(`SELECT created_by FROM lessons WHERE title = 'Signed-in lesson'`);
    check(createdBy === actor, "the lesson is attributed to the provider account's actor");
    await app.screenshot({ path: join(evidence, "m5-session.png"), fullPage: true });

    // Another origin on the same site tries to write with the cookie.
    const mark2 = pool.log.length;
    const evil = await context.newPage();
    await evil.goto(`http://localhost:${PORTS.evil}`);
    await evil.waitForFunction(() => document.title === "sent");
    await evil.waitForTimeout(300);
    const forged = pool.log.slice(mark2).find((e) => e.method === "POST" && e.path === "/api/v1/lessons");
    check(forged && forged.sessionCookie && forged.status === 403 && forged.code === "csrf_failed", `cross-origin write refused: ${JSON.stringify(forged)}`);
    check(psql(`SELECT count(*) FROM lessons WHERE title LIKE 'Forged%'`) === "0", "nothing forged was written");

    const cookieValue = (await context.cookies(origin)).find((c) => c.name === "__Host-trestle_session").value;
    await clickFor(app, app.getByRole("button", { name: "Sign out" }), "POST", /^\/api\/v1\/auth\/logout$/);
    await app.getByRole("heading", { name: "Sign in" }).waitFor();
    const replay = await fetch(`http://127.0.0.1:${PORTS.sessionApi}/api/v1/auth/session`, { headers: { Cookie: `__Host-trestle_session=${cookieValue}` } });
    check(replay.status === 401, "a signed-out session cookie is dead");
    const sessions = psql("SELECT count(*) || ' sessions, ' || count(revoked_at) || ' revoked' FROM identity_sessions");
    step("production identity: boot refusal, OIDC login, session writes, CSRF refusal, sign-out", {
      productionBootRefusal: refused,
      provisioning: { actor, provisioned },
      authorizationRequest: {
        response_type: authorize.response_type,
        scope: authorize.scope,
        code_challenge_method: authorize.code_challenge_method,
        redirect_uri: authorize.redirect_uri,
        stateLength: authorize.state.length,
        nonceLength: authorize.nonce.length,
      },
      tokenExchange: idp.events.filter((e) => e.event === "token"),
      cookies,
      signedInWrite: { status: signedWrite.status, csrfHeaderSent: true, createdBy },
      crossOriginWrite: forged,
      afterSignOut: { replayedCookieStatus: replay.status, sessions },
    });

    report.result = "passed";
  } catch (error) {
    report.result = "failed";
    report.error = String(error?.stack ?? error);
    throw error;
  } finally {
    await browser?.close();
    for (const child of [...children]) await stop(child);
    report.finishedAt = new Date().toISOString();
    report.edgeLog = pool.log;
    writeFileSync(join(evidence, "m5-demo.json"), `${JSON.stringify(report, null, 2)}\n`);
    for (const server of servers) {
      server.closeAllConnections?.();
      await new Promise((r) => (server.close.length ? server.close(r) : Promise.resolve(server.close()).then(r)));
    }
    try {
      sh("dropdb", ["--if-exists", "--force", db]);
    } catch (error) {
      log(`could not drop ${db}: ${error}`);
    }
  }
}

main().then(
  () => log("all checks passed; report in docs/evidence/m5-demo.json"),
  (error) => {
    console.error(error);
    process.exitCode = 1;
  },
);
