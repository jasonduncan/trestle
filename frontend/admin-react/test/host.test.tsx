import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { AdminHost } from "../src/host";

const session = vi.hoisted(() => ({ state: { kind: "loading" } as any, refresh: vi.fn(), signOut: vi.fn() }));
vi.mock("@trestle/interaction-react", async (original) => ({
  ...await original<object>(), useSession: () => session,
  loadMethods: async () => ({ password: false, oidc: false }),
}));
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const profile = { header: "Trestle-Tenant", label: "Workspace", path: "/api/workspaces" };
const manifest = { protocol: "trestle.views", protocolVersion: "1.0", views: [] };
const signedIn = () => { session.state = { kind: "signed-in", actor: "alice", csrfToken: "test-csrf", expiresAt: "later" }; };

describe("production host identity and workspace discovery", () => {
  it("shows signed-out state without dispatching protected reads", async () => {
    session.state = { kind: "signed-out" };
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    render(<AdminHost tenantProfile={profile} />);
    expect(await screen.findByText(/signed out/i)).toBeDefined();
    expect(fetch).not.toHaveBeenCalled();
  });
  it.each([0, 1, 2])("discovers %s eligible tenants without an owner rule", async (count) => {
    signedIn();
    const items = Array.from({ length: count }, (_, i) => ({ id: "tenant-" + i, name: "Workspace " + i, role: "staff" }));
    const fetch = vi.fn(async (path: string, init: RequestInit) => {
      expect(new Headers(init.headers).get("X-CSRF-Token")).toBe("test-csrf");
      expect(new Headers(init.headers).has("Authorization")).toBe(false);
      expect(init.cache).toBe("no-store");
      return Response.json(path === profile.path ? { items } : manifest);
    }); vi.stubGlobal("fetch", fetch);
    render(<AdminHost tenantProfile={profile} />);
    if (!count) expect(await screen.findByText("No eligible tenants are available.")).toBeDefined();
    else {
      expect(await screen.findByRole("combobox", { name: "Workspace" })).toBeDefined();
      await waitFor(() => expect(screen.getAllByRole("option")).toHaveLength(count));
      expect(await screen.findByText("No views available")).toBeDefined();
    }
  });
  it("quarantines the workspace after authoritative access refusal", async () => {
    signedIn();
    vi.stubGlobal("fetch", vi.fn(async (path: string) => path === profile.path ? Response.json({ items: [{ id: "tenant", name: "Workspace" }] }) : Response.json({ code: "permission_denied" }, { status: 403 })));
    render(<AdminHost tenantProfile={profile} developmentUsers={{ fallback: { token: "dev-only" } }} />);
    expect(await screen.findByText("Access needs refreshing")).toBeDefined();
    expect(screen.queryByRole("combobox")).toBeNull();
  });
  it("never falls back to a development identity after a failed session check", async () => {
    session.state = { kind: "error", message: "lost connection" };
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    render(<AdminHost tenantProfile={profile} developmentUsers={{ fallback: { token: "dev-only" } }} />);
    expect(await screen.findByRole("alert")).toBeDefined();
    expect(fetch).not.toHaveBeenCalled();
  });
});
