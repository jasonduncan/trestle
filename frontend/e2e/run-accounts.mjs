#!/usr/bin/env node
// Password accounts in a real browser, operated through the CLI.
//
//   1. An operator invites a teacher with `pilot account invite --send`
//      (mail goes to the file backend); the link from the message opens the
//      set-password page, which removes the token from the address bar.
//   2. Password rules are enforced; the teacher sets a password, lands
//      signed in, and creates a lesson.
//   3. Change password keeps this browser and signs another device out.
//   4. Sign-out, a failed sign-in, and sign-in with the new password.
//   5. An operator reset link (`reset-link --send`) ends every session.
//   6. An owner invited with a printed link uses the admin shell at /admin/.
//   7. Five failed attempts lock the account; the right password then fails
//      with the same message.
//
// Writes docs/evidence/accounts-demo.json and screenshots. Every process
// this script starts is tracked and stopped by its own PID.

import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer as createHttpServer, request as httpRequest } from "node:http";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const evidence = join(repo, "docs/evidence");
const API_PORT = 18401;
const EDGE_PORT = 18420;
const ORIGIN = `http://localhost:${EDGE_PORT}`;
const NORTH = "11111111-1111-4111-8111-111111111111";
const TEACHER = "teacher@example.com";
const OWNER = "owner@example.com";

const children = new Set();
const report = { demonstration: "Password accounts with operator-issued invite and reset links", startedAt: new Date().toISOString(), steps: [] };
const log = (m) => console.log(`[accounts-demo] ${m}`);
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
  for (let i = 0; i < 300; i++) {
    if (child.exitCode !== null) throw new Error(`${child.name} exited early:\n${child.errors}`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out waiting for ${url}`);
}

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml" };

/** One origin: product at /, admin at /admin/, /api and /_meta to the API. */
async function startEdge(product, admin) {
  const server = createHttpServer((req, res) => {
    const url = new URL(req.url, "http://edge");
    if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/_meta/")) {
      const upstream = httpRequest({ host: "127.0.0.1", port: API_PORT, method: req.method, path: req.url, headers: req.headers }, (up) => {
        res.writeHead(up.statusCode, up.headers);
        up.pipe(res);
      });
      upstream.on("error", () => {
        res.writeHead(502);
        res.end();
      });
      req.pipe(upstream);
      return;
    }
    const [root, path] = url.pathname.startsWith("/admin/") ? [admin, url.pathname.slice("/admin".length)] : [product, url.pathname];
    let file = normalize(join(root, decodeURIComponent(path)));
    if (!file.startsWith(root) || !existsSync(file) || statSync(file).isDirectory()) file = join(root, "index.html");
    res.writeHead(200, { "Content-Type": TYPES[extname(file)] ?? "application/octet-stream", "Cache-Control": "no-store" });
    res.end(readFileSync(file));
  });
  server.listen(EDGE_PORT, "127.0.0.1");
  await once(server, "listening");
  return server;
}

/** The set-password link from the newest message, decoding quoted-printable. */
function linkFromMailbox(mailbox, seen) {
  const files = readdirSync(mailbox).filter((f) => f.endsWith(".eml") && !seen.has(f)).sort();
  check(files.length === 1, `exactly one new message (found ${files.length})`);
  seen.add(files[0]);
  const raw = readFileSync(join(mailbox, files[0]), "utf8");
  const [headers, ...rest] = raw.split("\r\n\r\n");
  let body = rest.join("\r\n\r\n");
  if (/Content-Transfer-Encoding: quoted-printable/i.test(headers)) {
    body = body.replace(/=\r?\n/g, "").replace(/=([0-9A-F]{2})/g, (_, hex) => String.fromCharCode(parseInt(hex, 16)));
  }
  const link = body.match(/http:\/\/localhost:\d+\/set-password#token=[A-Za-z0-9_-]{43}/)?.[0];
  check(link, `a set-password link in the message:\n${body}`);
  const subject = headers.match(/^Subject: (.*)$/m)?.[1];
  const to = headers.match(/^To: (.*)$/m)?.[1];
  return { link, subject, to, file: files[0] };
}

async function main() {
  for (const port of [API_PORT, EDGE_PORT]) check(await portFree(port), `port ${port} is free`);
  mkdirSync(evidence, { recursive: true });
  report.release = {
    commit: sh("git", ["-C", repo, "rev-parse", "HEAD"]),
    uncommittedChanges: sh("git", ["-C", repo, "status", "--porcelain", "--", ".", ":!docs/evidence"]).length > 0,
  };

  log("building the API and both bundles …");
  sh("cargo", ["build", "-q", "-p", "pilot", "-p", "pilot-migrations"], { cwd: repo });
  const vite = join(repo, "frontend/node_modules/vite/bin/vite.js");
  for (const app of ["product", "admin"]) sh(process.execPath, [vite, "build", "--logLevel", "error"], { cwd: join(repo, "frontend", app) });
  const bin = join(repo, "target/debug");

  const work = mkdtempSync(join(tmpdir(), "trestle-accounts-"));
  const mailbox = join(work, "mailbox");
  const db = `trestle_accounts_${Date.now()}`;
  const dbEnv = { DATABASE_URL: `postgres://localhost/${db}`, RUST_LOG: "warn" };
  const operatorEnv = {
    ...dbEnv,
    TRESTLE_PUBLIC_ORIGIN: ORIGIN,
    TRESTLE_EMAIL_URL: `file://${mailbox}`,
    TRESTLE_EMAIL_FROM: "Trestle Pilot <no-reply@example.com>",
  };
  const pilot = (args) => sh(join(bin, "pilot"), args, { env: { ...process.env, ...operatorEnv }, cwd: work });
  const psql = (sql) => sh("psql", ["-d", db, "-qAtc", sql]);
  sh("createdb", [db]);
  let browser;
  let edge;
  try {
    sh(join(bin, "pilot-migrate"), [], { env: { ...process.env, ...dbEnv }, cwd: work });
    pilot(["seed-dev"]);
    const api = start("api (password sign-in)", join(bin, "pilot"), ["serve"], {
      env: {
        ...dbEnv,
        TRESTLE_ENV: "development",
        TRESTLE_BIND: `127.0.0.1:${API_PORT}`,
        TRESTLE_PASSWORD_LOGIN: "true",
        // Every browser in this run reaches the API from one address.
        TRESTLE_PASSWORD_ATTEMPTS_PER_MINUTE: "60",
        TRESTLE_SESSION_KEY: "demo session key, per run, at least 32 bytes long",
        TRESTLE_CURSOR_KEY: "demo cursor key, per run, at least 32 bytes long",
        TRESTLE_PUBLIC_ORIGIN: ORIGIN,
      },
      cwd: work,
    });
    await waitFor(`http://127.0.0.1:${API_PORT}/readyz`, api);
    edge = await startEdge(join(repo, "frontend/product/dist"), join(repo, "frontend/admin/dist"));
    browser = await chromium.launch({ channel: "chrome", headless: true });
    const seen = new Set();

    // 1. Invite by email; the link opens the set-password page.
    const invited = pilot(["account", "invite", "--email", TEACHER, "--school", NORTH, "--role", "teacher", "--send"]);
    const invite = linkFromMailbox(mailbox, seen);
    check(!invited.includes("#token="), "sending does not print the link");
    const laptop = await browser.newContext();
    const page = await laptop.newPage();
    await page.goto(invite.link);
    await page.getByRole("heading", { name: "Choose your password" }).waitFor();
    await page.getByText(TEACHER).waitFor();
    check(!page.url().includes("token"), `the token left the address bar: ${page.url()}`);
    await page.screenshot({ path: join(evidence, "accounts-set-password.png") });
    step("operator emails an invite; the link opens the set-password page", {
      operatorOutput: invited.split("\n"),
      message: { to: invite.to, subject: invite.subject },
      addressBarAfterLoad: page.url(),
    });

    // 2. Rules, then a good password; signed in; real work.
    await page.getByLabel("New password", { exact: true }).fill("password123");
    await page.getByLabel("Confirm password", { exact: true }).fill("password123");
    await page.getByRole("button", { name: "Set password" }).click();
    await page.getByText("This password is too common.").first().waitFor();
    const first = "maple syrup on snow days";
    await page.getByLabel("New password", { exact: true }).fill(first);
    await page.getByLabel("Confirm password", { exact: true }).fill(first);
    await Promise.all([page.waitForURL(`${ORIGIN}/`), page.getByRole("button", { name: "Set password" }).click()]);
    await page.getByText("Signed in").waitFor();
    await page.getByLabel("School").locator("option", { hasText: "Northside Elementary (teacher)" }).waitFor({ state: "attached" });
    await page.getByLabel("Title", { exact: true }).fill("Invited teacher's lesson");
    await page.getByLabel("Grade level", { exact: true }).fill("3");
    const [created] = await Promise.all([
      page.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname === "/api/v1/lessons"),
      page.getByRole("button", { name: "Create lesson" }).click(),
    ]);
    check(created.status() === 201, `lesson created (${created.status()})`);
    const cookies = (await laptop.cookies(ORIGIN)).map(({ name, httpOnly, secure, sameSite }) => ({ name, httpOnly, secure, sameSite }));
    step("password rules enforced; the teacher lands signed in and works", {
      rejected: "password123 (too common)",
      lessonStatus: created.status(),
      cookies,
    });

    // 3. A second device, then change password from the first.
    const phone = await browser.newContext();
    const other = await phone.newPage();
    await other.goto(ORIGIN);
    await other.getByLabel("Email").fill(TEACHER);
    await other.getByLabel("Password").fill(first);
    await other.getByRole("button", { name: "Sign in" }).click();
    await other.getByText("Signed in").waitFor();
    await page.getByRole("button", { name: "Change password" }).click();
    const second = "the second long passphrase";
    await page.getByLabel("Current password", { exact: true }).fill(first);
    await page.getByLabel("New password", { exact: true }).fill(second);
    await page.locator("form").filter({ has: page.getByLabel("Current password", { exact: true }) }).getByRole("button", { name: "Change password" }).click();
    await page.getByText("Password changed. Your other devices were signed out.").waitFor();
    await other.reload();
    await other.getByRole("heading", { name: "Sign in" }).waitFor();
    step("change password keeps this browser and signs the other device out", { otherDevice: "shown the sign-in form after reload" });

    // 4. Sign out, a failed sign-in, the new password.
    await page.getByRole("button", { name: "Sign out" }).click();
    await page.getByRole("heading", { name: "Sign in" }).waitFor();
    await page.getByLabel("Email").fill(TEACHER);
    await page.getByLabel("Password").fill(first);
    await page.getByRole("button", { name: "Sign in" }).click();
    const failure = await page.getByRole("alert").textContent();
    await page.screenshot({ path: join(evidence, "accounts-sign-in.png") });
    await page.getByLabel("Password").fill(second);
    await page.getByRole("button", { name: "Sign in" }).click();
    await page.getByText("Signed in").waitFor();
    step("sign-out, a failed sign-in with the old password, then the new one", { failureMessage: failure });

    // 5. Operator reset ends every session.
    const reset = pilot(["account", "reset-link", "--email", TEACHER, "--send"]);
    const resetMail = linkFromMailbox(mailbox, seen);
    const kiosk = await browser.newContext();
    const resetPage = await kiosk.newPage();
    await resetPage.goto(resetMail.link);
    await resetPage.getByRole("heading", { name: "Choose a new password" }).waitFor();
    const third = "a third passphrase after reset";
    await resetPage.getByLabel("New password", { exact: true }).fill(third);
    await resetPage.getByLabel("Confirm password", { exact: true }).fill(third);
    await Promise.all([resetPage.waitForURL(`${ORIGIN}/`), resetPage.getByRole("button", { name: "Set password" }).click()]);
    await resetPage.getByText("Signed in").waitFor();
    await page.reload();
    await page.getByRole("heading", { name: "Sign in" }).waitFor();
    const reused = await resetPage.request.post(`${ORIGIN}/api/v1/auth/password/inspect`, {
      headers: { Origin: ORIGIN, "Content-Type": "application/json" },
      data: { token: new URL(resetMail.link).hash.split("=")[1] },
    });
    check(reused.status() === 400, "the reset link worked once");
    step("operator reset link: new password, every earlier session ended, link spent", {
      operatorOutput: reset.split("\n"),
      message: { to: resetMail.to, subject: resetMail.subject },
      reusedLinkStatus: reused.status(),
    });

    // 6. An owner with a printed link uses the admin shell.
    const printed = pilot(["account", "invite", "--email", OWNER, "--school", NORTH, "--role", "owner"]);
    const ownerLink = printed.match(/http:\/\/localhost:\d+\/set-password#token=[A-Za-z0-9_-]{43}/)?.[0];
    check(ownerLink, "a printed invite link");
    const desk = await (await browser.newContext()).newPage();
    await desk.goto(ownerLink);
    const ownerPassword = "quiet evening in the admin shell";
    await desk.getByLabel("New password", { exact: true }).fill(ownerPassword);
    await desk.getByLabel("Confirm password", { exact: true }).fill(ownerPassword);
    await Promise.all([desk.waitForURL(`${ORIGIN}/`), desk.getByRole("button", { name: "Set password" }).click()]);
    await desk.goto(`${ORIGIN}/admin/`);
    await desk.getByRole("heading", { name: "Lessons" }).waitFor();
    await desk.getByRole("cell", { name: "Invited teacher's lesson", exact: true }).waitFor();
    await desk.screenshot({ path: join(evidence, "accounts-admin.png"), fullPage: true });
    step("an owner invited by printed link uses the admin shell on the same origin", { adminUrl: `${ORIGIN}/admin/` });

    // 7. Lockout.
    const attempt = (password) =>
      fetch(`http://127.0.0.1:${API_PORT}/api/v1/auth/password/login`, {
        method: "POST",
        headers: { Origin: ORIGIN, "Content-Type": "application/json" },
        body: JSON.stringify({ email: OWNER, password }),
      }).then(async (r) => ({ status: r.status, body: await r.json() }));
    const failures = [];
    for (let i = 0; i < 5; i++) failures.push((await attempt(`wrong guess number ${i}`)).status);
    const lockedOut = await attempt(ownerPassword);
    check(lockedOut.status === 401 && lockedOut.body.code === "login_failed", "right password refused while locked");
    const accounts = pilot(["account", "list"]).split("\n");
    check(accounts.some((l) => l.startsWith("locked") && l.endsWith(OWNER)), `owner shown as locked: ${accounts}`);
    step("five failures lock the account; the right password then fails the same way", {
      failures,
      whileLocked: { status: lockedOut.status, code: lockedOut.body.code },
      accountList: accounts,
      storedHashes: psql("SELECT count(*) || ' of ' || (SELECT count(*) FROM identity_accounts) || ' hashes are argon2id' FROM identity_accounts WHERE password_hash LIKE '$argon2id$v=19$m=19456,t=2,p=1$%'"),
    });

    report.result = "passed";
  } catch (error) {
    report.result = "failed";
    report.error = String(error?.stack ?? error);
    throw error;
  } finally {
    await browser?.close();
    edge?.closeAllConnections?.();
    await new Promise((r) => (edge ? edge.close(r) : r()));
    for (const child of [...children]) await stop(child);
    report.finishedAt = new Date().toISOString();
    writeFileSync(join(evidence, "accounts-demo.json"), `${JSON.stringify(report, null, 2)}\n`);
    try {
      sh("dropdb", ["--if-exists", "--force", db]);
    } catch (error) {
      log(`could not drop ${db}: ${error}`);
    }
    rmSync(work, { recursive: true, force: true });
  }
}

main().then(
  () => log("all checks passed; report in docs/evidence/accounts-demo.json"),
  (error) => {
    console.error(error);
    process.exitCode = 1;
  },
);
