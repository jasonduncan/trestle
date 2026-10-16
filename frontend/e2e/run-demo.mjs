#!/usr/bin/env node
// M2 end-to-end demonstration in a real browser (design doc §18 M2, §19.2 A).
//
//   1. Release A (git tag m2-release-a) serves; the built React bundle renders
//      its form and creates a lesson.
//   2. Release B's expand migration runs while A is still serving.
//   3. Rolling deploy: A stops, B starts. The page that still shows A's form
//      submits (retained revision, accepted by B).
//   4. The same bundle, reloaded without rebuilding, renders B's new optional
//      field and submits it.
//   5. Permission revoked between load and submit is rejected.
//   6. Two tenants stay isolated; an unaccepted revision gets 409.
//
// Writes docs/evidence/m2-demo.json and screenshots. Every process this
// script starts is tracked and stopped by its own PID; nothing else is touched.

import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const evidence = join(repo, "docs/evidence");
const RELEASE_A = process.env.RELEASE_A ?? "m2-release-a";
const API_PORT = Number(process.env.E2E_API_PORT ?? 8031);
const WEB_PORT = Number(process.env.E2E_WEB_PORT ?? 4174);
const API = `http://127.0.0.1:${API_PORT}`;
const WEB = `http://127.0.0.1:${WEB_PORT}`;
const NORTH = "11111111-1111-4111-8111-111111111111";
const SOUTH = "22222222-2222-4222-8222-222222222222";
const ALICE = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const DEV_TOKENS = [
  "alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
  "carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc",
].join(",");

const children = new Set();
const report = { demonstration: "M2: runtime-rendered form across a backend release", startedAt: new Date().toISOString(), steps: [] };

function log(message) {
  console.log(`[m2-demo] ${message}`);
}

function sh(cmd, args, options = {}) {
  return execFileSync(cmd, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], ...options }).trim();
}

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

function digestDir(dir) {
  const hash = createHash("sha256");
  const walk = (d) =>
    readdirSync(d)
      .sort()
      .flatMap((f) => (statSync(join(d, f)).isDirectory() ? walk(join(d, f)) : [join(d, f)]));
  for (const file of walk(dir)) {
    hash.update(relative(dir, file));
    hash.update(readFileSync(file));
  }
  return `sha256:${hash.digest("hex")}`;
}

function buildRelease(ref) {
  const tree = join(repo, "target/e2e/release-a");
  if (!existsSync(tree)) sh("git", ["-C", repo, "worktree", "add", "--detach", tree, ref]);
  else sh("git", ["-C", tree, "checkout", "--detach", "--quiet", ref]);
  const commit = sh("git", ["-C", tree, "rev-parse", "HEAD"]);
  log(`building release A (${ref} = ${commit.slice(0, 7)}) …`);
  const target = join(repo, "target/e2e/release-a-target");
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: tree, env: { ...process.env, CARGO_TARGET_DIR: target } });
  return { ref, commit, bin: join(target, "debug") };
}

function buildCurrent() {
  const commit = sh("git", ["-C", repo, "rev-parse", "HEAD"]);
  // The demo's own output directory does not count as a source change.
  const dirty = sh("git", ["-C", repo, "status", "--porcelain", "--", ".", ":!docs/evidence"]).length > 0;
  log(`building release B (${commit.slice(0, 7)}${dirty ? ", uncommitted changes" : ""}) …`);
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: repo });
  return { ref: "HEAD", commit, dirty, bin: join(repo, "target/debug") };
}

async function createViaForm(page, { title, grade, subtitle }) {
  await page.getByLabel("Title", { exact: true }).fill(title);
  await page.getByLabel("Grade level", { exact: true }).fill(String(grade));
  if (subtitle !== undefined) await page.getByLabel("Subtitle (optional)").fill(subtitle);
  const [response] = await Promise.all([
    page.waitForResponse((r) => r.url() === `${WEB}/api/v1/lessons` && r.request().method() === "POST"),
    page.getByRole("button", { name: "Create lesson" }).click(),
  ]);
  return {
    status: response.status(),
    revisionHeader: response.request().headers()["trestle-interaction-revision"],
    payload: response.request().postDataJSON(),
    body: await response.json(),
  };
}

async function main() {
  check(await portFree(API_PORT), `port ${API_PORT} is free`);
  check(await portFree(WEB_PORT), `port ${WEB_PORT} is free`);
  mkdirSync(evidence, { recursive: true });

  const releaseA = buildRelease(RELEASE_A);
  const releaseB = buildCurrent();
  report.releases = {
    A: { ref: releaseA.ref, commit: releaseA.commit },
    B: { ref: releaseB.ref, commit: releaseB.commit, uncommittedChanges: releaseB.dirty },
  };

  log("building the React bundle once …");
  sh(process.execPath, [join(repo, "frontend/node_modules/vite/bin/vite.js"), "build", "--logLevel", "error"], {
    cwd: join(repo, "frontend/product"),
  });
  const bundleDigest = digestDir(join(repo, "frontend/product/dist"));
  report.bundle = { digest: bundleDigest, builtOnce: true };

  const db = `trestle_e2e_${Date.now()}`;
  const url = `postgres://localhost/${db}`;
  const psql = (sql) => sh("psql", ["-d", db, "-qAtc", sql]);
  sh("createdb", [db]);
  report.database = db;
  const serverEnv = { DATABASE_URL: url, TRESTLE_ENV: "development", TRESTLE_BIND: `127.0.0.1:${API_PORT}`, TRESTLE_DEV_TOKENS: DEV_TOKENS, RUST_LOG: "warn" };

  let api;
  let web;
  let browser;
  try {
    sh(join(releaseA.bin, "pilot-migrate"), [], { env: { ...process.env, DATABASE_URL: url }, cwd: tmpdir() });
    sh(join(releaseA.bin, "pilot"), ["seed-dev"], { env: { ...process.env, ...serverEnv }, cwd: tmpdir() });
    api = start("release A api", join(releaseA.bin, "pilot"), ["serve"], { env: serverEnv });
    await waitFor(`${API}/readyz`, api);

    web = start("vite preview", process.execPath, [join(repo, "frontend/node_modules/vite/bin/vite.js"), "preview", "--host", "127.0.0.1", "--port", String(WEB_PORT), "--strictPort"], {
      cwd: join(repo, "frontend/product"),
      env: { TRESTLE_API: API },
    });
    await waitFor(WEB, web);

    browser = await chromium.launch({ channel: "chrome", headless: true });
    const page = await browser.newPage();
    // Routing every request disables Playwright's HTTP cache, so the reload
    // re-downloads each asset and the byte comparison below is about what
    // the server serves, not what a cache kept.
    await page.route("**/*", (route) => route.continue());
    const assets = new Map();
    let pending = [];
    page.on("response", (r) => {
      if (/\/assets\/.+\.(js|css)$/.test(r.url())) {
        pending.push(
          r.body().then(
            (body) => assets.set(new URL(r.url()).pathname, createHash("sha256").update(body).digest("hex")),
            () => {},
          ),
        );
      }
    });
    const settledAssets = async () => {
      await Promise.all(pending);
      pending = [];
      return Object.fromEntries([...assets].sort());
    };

    // 1. Release A.
    await page.goto(WEB);
    await page.getByLabel("Title", { exact: true }).waitFor();
    const labelsA = await page.locator(".trestle-field label").allInnerTexts();
    const assetsA = await settledAssets();
    await page.screenshot({ path: join(evidence, "m2-release-a.png"), fullPage: true });
    const a = await createViaForm(page, { title: "Fractions (release A)", grade: 4 });
    check(a.status === 201, `release A create returned ${a.status}`);
    check(!labelsA.some((l) => l.startsWith("Subtitle")), "release A has no subtitle field");
    const revisionA = a.revisionHeader;
    step("release A renders and submits", { labels: labelsA, revision: revisionA, request: a.payload, status: a.status, response: a.body });

    // 2. Expand migration while A still serves.
    const applied = sh(join(releaseB.bin, "pilot-migrate"), [], { env: { ...process.env, DATABASE_URL: url }, cwd: tmpdir() });
    const stillServing = await fetch(`${API}/api/v1/lessons`, {
      method: "POST",
      headers: { Authorization: "Bearer alice-dev-token", "Trestle-Tenant": NORTH, "Content-Type": "application/json" },
      body: JSON.stringify({ title: "API call during expand", grade_level: 3 }),
    });
    check(stillServing.status === 201, "release A keeps working after B's expand migration");
    step("B's expand migration applied while A serves", { migrationOutput: applied, releaseAStatusAfterExpand: stillServing.status });

    // 3. Rolling deploy; the open page still shows A's form.
    await stop(api);
    api = start("release B api", join(releaseB.bin, "pilot"), ["serve"], { env: serverEnv });
    await waitFor(`${API}/readyz`, api);
    const openForm = await createViaForm(page, { title: "Fractions (A form, B server)", grade: 5 });
    check(openForm.revisionHeader === revisionA, "the open page still submits release A's revision");
    check(openForm.status === 201, `B accepted the retained A revision (got ${openForm.status})`);
    check(openForm.body.subtitle === null, "B stores no subtitle for an A submission");
    step("open release A form submits to release B", { revision: openForm.revisionHeader, request: openForm.payload, status: openForm.status, response: openForm.body });

    // 4. Same bundle, reloaded: renders and submits B's new field.
    assets.clear();
    await page.reload();
    await page.getByLabel("Subtitle (optional)").waitFor();
    const labelsB = await page.locator(".trestle-field label").allInnerTexts();
    const assetsB = await settledAssets();
    check(Object.keys(assetsA).length > 0, "assets were captured on first load");
    check(
      JSON.stringify(assetsA) === JSON.stringify(assetsB),
      `the reloaded page loaded byte-identical assets: ${JSON.stringify({ assetsA, assetsB })}`,
    );
    check(digestDir(join(repo, "frontend/product/dist")) === bundleDigest, "the bundle on disk was not rebuilt");
    await page.screenshot({ path: join(evidence, "m2-release-b.png"), fullPage: true });
    const b = await createViaForm(page, { title: "Ratios (release B)", grade: 6, subtitle: "Part-to-part" });
    check(b.status === 201 && b.body.subtitle === "Part-to-part", "release B stores the new field");
    check(b.revisionHeader !== revisionA, "the reloaded form uses B's revision");
    await page.getByRole("cell", { name: "Part-to-part" }).waitFor();
    step("same bundle renders and submits the new optional field", {
      bundleDigest,
      assets: assetsB,
      labels: labelsB,
      revision: b.revisionHeader,
      request: b.payload,
      status: b.status,
      response: b.body,
    });

    // 5. Revoke permission between load and submit.
    const before = Number(psql("SELECT count(*) FROM lessons"));
    psql(`UPDATE school_memberships SET role = 'viewer' WHERE actor_id = '${ALICE}' AND school_id = '${NORTH}'`);
    const revoked = await createViaForm(page, { title: "After revocation", grade: 7 });
    await page.getByText("You don't have permission to do this.").waitFor();
    const after = Number(psql("SELECT count(*) FROM lessons"));
    check(revoked.status === 403 && revoked.body.code === "permission_denied", "revoked submit is refused");
    check(after === before, "nothing was written");
    psql(`UPDATE school_memberships SET role = 'teacher' WHERE actor_id = '${ALICE}' AND school_id = '${NORTH}'`);
    step("permission revoked after load is rejected at submit", { status: revoked.status, code: revoked.body.code, lessonsBefore: before, lessonsAfter: after });

    // 6. Two tenants and an unaccepted revision.
    await page.getByLabel("User").selectOption("bob");
    await page.getByLabel("School").selectOption(SOUTH);
    await page.getByLabel("Title", { exact: true }).waitFor();
    const bob = await createViaForm(page, { title: "Fractions (release A)", grade: 4 });
    check(bob.status === 201, "the same title is allowed in another school");
    const probe = await page.evaluate(
      async ({ id, north, south }) => {
        const get = (tenant) =>
          fetch(`/api/v1/lessons/${id}`, { headers: { Authorization: "Bearer bob-dev-token", "Trestle-Tenant": tenant } }).then((r) => r.status);
        const stale = await fetch("/api/v1/lessons", {
          method: "POST",
          headers: {
            Authorization: "Bearer bob-dev-token",
            "Trestle-Tenant": south,
            "Content-Type": "application/json",
            "Trestle-Interaction-Revision": "r-0000000000000000",
          },
          body: JSON.stringify({ title: "Stale", grade_level: 3 }),
        });
        return { ownSchool: await get(south), claimedSchool: await get(north), stale: stale.status, staleCode: (await stale.json()).code };
      },
      { id: a.body.id, north: NORTH, south: SOUTH },
    );
    check(probe.ownSchool === 404 && probe.claimedSchool === 403, "bob cannot read alice's lesson");
    check(probe.stale === 409 && probe.staleCode === "interaction_stale", "unaccepted revision is refused");
    step("two tenants stay isolated; unaccepted revisions get 409", { bobCreate: bob.status, ...probe });

    report.result = "passed";
  } catch (error) {
    report.result = "failed";
    report.error = String(error?.stack ?? error);
    throw error;
  } finally {
    report.finishedAt = new Date().toISOString();
    writeFileSync(join(evidence, "m2-demo.json"), `${JSON.stringify(report, null, 2)}\n`);
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
  () => log("all checks passed; report in docs/evidence/m2-demo.json"),
  (error) => {
    console.error(error);
    process.exitCode = 1;
  },
);
