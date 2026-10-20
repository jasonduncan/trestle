import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { decodeView, type Reply, type Transport } from "@trestle-framework/client";
import { AdminShell, CollectionView } from "../src/index";
afterEach(cleanup);
const fixture = JSON.parse(readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../../../fixtures/views/support.json"), "utf8"));
const reply = (items: unknown[], next: string | null = null): Reply => ({ kind: "response", status: 200, bytes: 0, body: { items, next_cursor: next } });
const row = (subject: string) => ({ id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", subject, account: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", state: "open", assignee: null, version: 1, created_at: "2026-10-18T12:00:00Z" });

it("resets paging and discards a delayed result after a filter change", async () => {
  let resolve!: (reply: Reply) => void;
  const list = vi.fn().mockResolvedValueOnce(reply([row("First")], "page-2"))
    .mockImplementationOnce(() => new Promise<Reply>((r) => { resolve = r; }))
    .mockResolvedValue(reply([row("Filtered")]));
  const transport = { list } as unknown as Transport;
  const view = decodeView(fixture.admin.views.find((v: any) => v.viewId === "support.case.collection"));
  render(<CollectionView view={view} transport={transport} navigate={() => {}} />);
  await screen.findByText("First"); fireEvent.click(screen.getByRole("button", { name: "Next", exact: true }));
  await screen.findByText("Loading records…");
  fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "filtered" } });
  await screen.findByText("Filtered");
  expect(list.mock.calls.at(-1)?.[1]).toEqual({ limit: "20", q: "filtered" });
  await act(async () => resolve(reply([row("Late old page")])));
  expect(screen.queryByText("Late old page")).toBeNull();
  expect(screen.getByText("Page 1")).toBeDefined();
});

it("reports an old backend's unsupported view surface without invocation", async () => {
  const transport = { list: vi.fn(async () => ({ kind: "response", status: 404, bytes: 0, body: {} })), submit: vi.fn() } as unknown as Transport;
  render(<AdminShell transport={transport} />);
  expect(await screen.findByText(/This server does not publish application views/)).toBeDefined();
  expect(transport.submit).not.toHaveBeenCalled();
});
