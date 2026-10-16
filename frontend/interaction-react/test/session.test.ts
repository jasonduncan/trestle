import { describe, expect, it, vi } from "vitest";

import { CSRF_HEADER, loadSession, loginUrl, sessionHeaders, signOut } from "../src/session";

const reply = (status: number, body?: unknown) =>
  new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

describe("browser sessions", () => {
  it("distinguishes signed in, signed out and unconfigured", async () => {
    const signedIn = await loadSession(async () =>
      reply(200, { actor: "a-1", csrf_token: "tok", expires_at: "2026-09-27T00:00:00Z" }),
    );
    expect(signedIn).toEqual({ kind: "signed-in", actor: "a-1", csrfToken: "tok", expiresAt: "2026-09-27T00:00:00Z" });
    expect(await loadSession(async () => reply(401, { code: "credentials_missing" }))).toEqual({ kind: "signed-out" });
    expect(await loadSession(async () => reply(404, { code: "not_found" }))).toEqual({ kind: "unavailable" });
    expect((await loadSession(async () => reply(200, { actor: 1 }))).kind).toBe("error");
    expect((await loadSession(async () => Promise.reject(new Error("offline")))).kind).toBe("error");
  });

  it("asks for the session with same-origin credentials and no caching", async () => {
    const fetchImpl = vi.fn(async () => reply(401));
    await loadSession(fetchImpl);
    expect(fetchImpl).toHaveBeenCalledWith("/api/v1/auth/session", expect.objectContaining({ credentials: "same-origin", cache: "no-store" }));
  });

  it("returns only to paths on this site", () => {
    expect(loginUrl("/lessons?id=1")).toBe("/api/v1/auth/login?return_to=%2Flessons%3Fid%3D1");
    for (const bad of ["https://evil.example/", "//evil.example", "/\\evil", "lessons"]) {
      expect(loginUrl(bad)).toBe("/api/v1/auth/login?return_to=%2F");
    }
  });

  it("sends the CSRF token to sign out and on requests", async () => {
    const fetchImpl = vi.fn(async () => reply(204));
    expect(await signOut("tok", fetchImpl)).toBe(true);
    expect(fetchImpl).toHaveBeenCalledWith(
      "/api/v1/auth/logout",
      expect.objectContaining({ method: "POST", credentials: "same-origin", headers: { [CSRF_HEADER]: "tok" } }),
    );
    expect(sessionHeaders({ kind: "signed-in", actor: "a", csrfToken: "tok", expiresAt: "" })).toEqual({ [CSRF_HEADER]: "tok" });
    expect(sessionHeaders({ kind: "signed-out" })).toEqual({});
  });
});
