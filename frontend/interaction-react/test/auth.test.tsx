// Password sign-in, set-password (invite/reset links) and change-password.

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ChangePassword, SetPassword, SignIn } from "../src/auth";

/** Text of the elements an input's aria-describedby points at. */
function description(element: HTMLElement): string {
  return (element.getAttribute("aria-describedby") ?? "")
    .split(" ")
    .map((id) => document.getElementById(id)?.textContent ?? "")
    .join(" ");
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const json = (status: number, body: unknown) =>
  new Response(body === null ? null : JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

/** A fetch stub answering by path; records every call. */
function server(routes: Record<string, (body: any) => Response>) {
  const calls: { path: string; init: RequestInit; body: any }[] = [];
  const fetchStub = vi.fn(async (path: string, init: RequestInit = {}) => {
    const body = init.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ path, init, body });
    const route = routes[path];
    if (!route) return json(404, { code: "not_found" });
    return route(body);
  });
  vi.stubGlobal("fetch", fetchStub);
  return calls;
}

describe("SignIn", () => {
  it("signs in with email and password and reports failures from the server", async () => {
    const onSignedIn = vi.fn();
    let attempts = 0;
    const calls = server({
      "/api/v1/auth/password/login": () =>
        ++attempts === 1
          ? json(401, { code: "login_failed", detail: "The email or password is not right." })
          : json(200, { actor: "a-1", csrf_token: "tok", expires_at: "" }),
    });
    const user = userEvent.setup();
    render(<SignIn methods={{ password: true, oidc: false }} onSignedIn={onSignedIn} />);
    await user.type(screen.getByLabelText("Email"), "teacher@example.com");
    await user.type(screen.getByLabelText("Password"), "wrong password here");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    expect((await screen.findByRole("alert")).textContent).toContain("The email or password is not right.");
    expect(onSignedIn).not.toHaveBeenCalled();

    await user.clear(screen.getByLabelText("Password"));
    await user.type(screen.getByLabelText("Password"), "the right password");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(onSignedIn).toHaveBeenCalledOnce());
    expect(calls[1]!.body).toEqual({ email: "teacher@example.com", password: "the right password" });
    expect(calls[1]!.init).toMatchObject({ method: "POST", credentials: "same-origin" });
    expect((screen.getByLabelText("Password") as HTMLInputElement).value).toBe("");
    expect(screen.queryByRole("link", { name: /organization/ })).toBeNull();
  });

  it("offers the identity provider when one is configured", () => {
    render(<SignIn methods={{ password: false, oidc: true }} onSignedIn={() => {}} returnTo="/admin/" />);
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(screen.getByRole("link", { name: "Sign in with your organization" }).getAttribute("href")).toBe(
      "/api/v1/auth/login?return_to=%2Fadmin%2F",
    );
  });

  it("explains throttling", async () => {
    server({ "/api/v1/auth/password/login": () => json(429, { code: "rate_limited" }) });
    const user = userEvent.setup();
    render(<SignIn methods={{ password: true, oidc: false }} onSignedIn={() => {}} />);
    await user.type(screen.getByLabelText("Email"), "a@example.com");
    await user.type(screen.getByLabelText("Password"), "whatever it is");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Too many attempts");
  });
});

describe("SetPassword", () => {
  it("takes the token from the fragment, clears it, and sets the password", async () => {
    window.history.replaceState(null, "", "/set-password#token=link-secret");
    const onDone = vi.fn();
    let tries = 0;
    const calls = server({
      "/api/v1/auth/password/inspect": () => json(200, { email: "new@example.com", purpose: "invite", expires_at: "" }),
      "/api/v1/auth/password/set": () =>
        ++tries === 1
          ? json(422, { code: "validation_failed", errors: [{ pointer: "/password", code: "too_common", message: "This password is too common." }] })
          : json(200, { actor: "a-1", csrf_token: "tok", expires_at: "" }),
    });
    const user = userEvent.setup();
    render(<SetPassword onDone={onDone} />);
    await screen.findByRole("heading", { name: "Choose your password" });
    screen.getByText("new@example.com");
    expect(window.location.hash).toBe("");
    expect(window.location.pathname).toBe("/set-password");
    expect(calls[0]!.body).toEqual({ token: "link-secret" });

    // Mismatched confirmation never reaches the server.
    await user.type(screen.getByLabelText("New password"), "password123");
    await user.type(screen.getByLabelText("Confirm password"), "password124");
    await user.click(screen.getByRole("button", { name: "Set password" }));
    expect((await screen.findByRole("alert")).textContent).toContain("don't match");
    expect(calls).toHaveLength(1);

    await user.clear(screen.getByLabelText("Confirm password"));
    await user.type(screen.getByLabelText("Confirm password"), "password123");
    await user.click(screen.getByRole("button", { name: "Set password" }));
    const field = screen.getByLabelText("New password");
    await waitFor(() => expect(field.getAttribute("aria-invalid")).toBe("true"));
    expect(description(field)).toContain("This password is too common.");

    await user.clear(field);
    await user.type(field, "maple syrup on snow days");
    await user.clear(screen.getByLabelText("Confirm password"));
    await user.type(screen.getByLabelText("Confirm password"), "maple syrup on snow days");
    await user.click(screen.getByRole("button", { name: "Set password" }));
    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(calls.at(-1)!.body).toEqual({ token: "link-secret", password: "maple syrup on snow days" });
  });

  it("says plainly when a link is no longer valid", async () => {
    window.history.replaceState(null, "", "/set-password#token=used-up");
    server({ "/api/v1/auth/password/inspect": () => json(400, { code: "token_invalid" }) });
    render(<SetPassword onDone={() => {}} />);
    await screen.findByRole("heading", { name: "This link can't be used" });
  });
});

describe("ChangePassword", () => {
  it("sends the CSRF token and maps field problems", async () => {
    const onDone = vi.fn();
    let tries = 0;
    const calls = server({
      "/api/v1/auth/password/change": () =>
        ++tries === 1
          ? json(422, { code: "validation_failed", errors: [{ pointer: "/current_password", code: "incorrect", message: "Your current password is not right." }] })
          : json(204, null),
    });
    const user = userEvent.setup();
    render(<ChangePassword csrfToken="csrf-1" onDone={onDone} />);
    await user.type(screen.getByLabelText("Current password"), "not it");
    await user.type(screen.getByLabelText("New password"), "another long passphrase");
    await user.click(screen.getByRole("button", { name: "Change password" }));
    await waitFor(() => expect(screen.getByLabelText("Current password").getAttribute("aria-invalid")).toBe("true"));
    expect((calls[0]!.init.headers as Record<string, string>)["X-CSRF-Token"]).toBe("csrf-1");

    await user.clear(screen.getByLabelText("Current password"));
    await user.type(screen.getByLabelText("Current password"), "the current one");
    await user.click(screen.getByRole("button", { name: "Change password" }));
    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
  });
});
