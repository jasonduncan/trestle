// Browser sessions (design doc §12.2, ADR-005). The server keeps the
// session; the browser holds only an HttpOnly cookie that scripts cannot
// read. The CSRF token comes from the session endpoint, lives in memory
// only (never browser storage) and is sent on every request; the server
// requires it, with an allowed Origin, on unsafe ones.

import { useCallback, useEffect, useState } from "react";

export const SESSION_PATH = "/api/v1/auth/session";
export const LOGIN_PATH = "/api/v1/auth/login";
export const LOGOUT_PATH = "/api/v1/auth/logout";
export const CSRF_HEADER = "X-CSRF-Token";

export type SessionState =
  | { kind: "loading" }
  | { kind: "signed-in"; actor: string; csrfToken: string; expiresAt: string }
  | { kind: "signed-out" }
  /** The server has no browser login configured (development tokens). */
  | { kind: "unavailable" }
  | { kind: "error"; message: string };

type Fetch = typeof fetch;

export async function loadSession(fetchImpl: Fetch = (i, init) => fetch(i, init)): Promise<SessionState> {
  let response: Response;
  try {
    response = await fetchImpl(SESSION_PATH, {
      credentials: "same-origin",
      cache: "no-store",
      headers: { Accept: "application/json" },
    });
  } catch (error) {
    return { kind: "error", message: error instanceof Error ? error.message : String(error) };
  }
  if (response.status === 404) return { kind: "unavailable" };
  if (response.status === 401) return { kind: "signed-out" };
  if (!response.ok) return { kind: "error", message: `The session check failed (${response.status}).` };
  const body: unknown = await response.json().catch(() => null);
  const record = typeof body === "object" && body !== null ? (body as Record<string, unknown>) : {};
  if (typeof record.actor !== "string" || typeof record.csrf_token !== "string") {
    return { kind: "error", message: "The session response was not understood." };
  }
  return {
    kind: "signed-in",
    actor: record.actor,
    csrfToken: record.csrf_token,
    expiresAt: typeof record.expires_at === "string" ? record.expires_at : "",
  };
}

/** Where to send the browser to sign in; it returns to `returnTo` (a path on this site). */
export function loginUrl(returnTo: string): string {
  const path = returnTo.startsWith("/") && !returnTo.startsWith("//") && !returnTo.includes("\\") ? returnTo : "/";
  return `${LOGIN_PATH}?${new URLSearchParams({ return_to: path }).toString()}`;
}

export async function signOut(csrfToken: string, fetchImpl: Fetch = (i, init) => fetch(i, init)): Promise<boolean> {
  try {
    const response = await fetchImpl(LOGOUT_PATH, {
      method: "POST",
      credentials: "same-origin",
      headers: { [CSRF_HEADER]: csrfToken },
    });
    return response.status === 204;
  } catch {
    return false;
  }
}

/** Headers a transport adds for a signed-in session. */
export function sessionHeaders(state: SessionState): Record<string, string> {
  return state.kind === "signed-in" ? { [CSRF_HEADER]: state.csrfToken } : {};
}

/** The current session, refreshed on demand (e.g. after a 401). */
export function useSession(fetchImpl?: Fetch) {
  const [state, setState] = useState<SessionState>({ kind: "loading" });
  const refresh = useCallback(async () => setState(await loadSession(fetchImpl)), [fetchImpl]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const end = useCallback(async () => {
    if (state.kind === "signed-in") await signOut(state.csrfToken, fetchImpl);
    await refresh();
  }, [state, fetchImpl, refresh]);
  return { state, refresh, signOut: end };
}

// Password accounts -----------------------------------------------------------

export const METHODS_PATH = "/api/v1/auth/methods";
export const PASSWORD_LOGIN_PATH = "/api/v1/auth/password/login";
export const PASSWORD_INSPECT_PATH = "/api/v1/auth/password/inspect";
export const PASSWORD_SET_PATH = "/api/v1/auth/password/set";
export const PASSWORD_CHANGE_PATH = "/api/v1/auth/password/change";

export interface SignInMethods {
  password: boolean;
  oidc: boolean;
}

/** What the sign-in screen can offer; `null` when browser sign-in is off. */
export async function loadMethods(fetchImpl: Fetch = (i, init) => fetch(i, init)): Promise<SignInMethods | null> {
  try {
    const response = await fetchImpl(METHODS_PATH, { credentials: "same-origin", cache: "no-store" });
    if (!response.ok) return null;
    const body = (await response.json()) as Record<string, unknown>;
    return { password: body.password === true, oidc: body.oidc === true };
  } catch {
    return null;
  }
}

export interface FieldProblem {
  pointer: string;
  code: string;
  message: string;
}

/** The result of an auth call: ok, or a message plus any field problems. */
export type AuthResult<T = unknown> =
  | { ok: true; value: T }
  | { ok: false; status: number; code?: string; message: string; errors: FieldProblem[] };

async function postJson<T>(
  path: string,
  body: unknown,
  headers: Record<string, string> = {},
  fetchImpl: Fetch = (i, init) => fetch(i, init),
): Promise<AuthResult<T>> {
  let response: Response;
  try {
    response = await fetchImpl(path, {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      headers: { "Content-Type": "application/json", Accept: "application/json", ...headers },
      body: JSON.stringify(body),
    });
  } catch {
    return { ok: false, status: 0, message: "The server could not be reached. Check your connection and try again.", errors: [] };
  }
  const text = await response.text();
  let parsed: unknown = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = null;
  }
  if (response.ok) return { ok: true, value: parsed as T };
  const problem = typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>) : {};
  const errors = Array.isArray(problem.errors) ? (problem.errors as FieldProblem[]) : [];
  const message =
    response.status === 429
      ? "Too many attempts. Wait a minute and try again."
      : typeof problem.detail === "string"
        ? problem.detail
        : `The request failed (${response.status}).`;
  return { ok: false, status: response.status, code: typeof problem.code === "string" ? problem.code : undefined, message, errors };
}

export function passwordLogin(email: string, password: string, fetchImpl?: Fetch) {
  return postJson<{ actor: string; csrf_token: string }>(PASSWORD_LOGIN_PATH, { email, password }, {}, fetchImpl);
}

export function inspectLink(token: string, fetchImpl?: Fetch) {
  return postJson<{ email: string; purpose: "invite" | "reset"; expires_at: string }>(PASSWORD_INSPECT_PATH, { token }, {}, fetchImpl);
}

export function setPassword(token: string, password: string, fetchImpl?: Fetch) {
  return postJson<{ actor: string; csrf_token: string }>(PASSWORD_SET_PATH, { token, password }, {}, fetchImpl);
}

export function changePassword(csrfToken: string, currentPassword: string, newPassword: string, fetchImpl?: Fetch) {
  return postJson<null>(
    PASSWORD_CHANGE_PATH,
    { current_password: currentPassword, new_password: newPassword },
    { [CSRF_HEADER]: csrfToken },
    fetchImpl,
  );
}

/** The link token from `#token=…` (no side effects). */
export function linkToken(win: Pick<Window, "location"> = window): string | null {
  return new URLSearchParams(win.location.hash.replace(/^#/, "")).get("token");
}

/** Removes the fragment from the address bar and history entry. */
export function forgetLinkToken(win: Pick<Window, "location" | "history"> = window): void {
  if (win.location.hash) win.history.replaceState(null, "", win.location.pathname + win.location.search);
}
