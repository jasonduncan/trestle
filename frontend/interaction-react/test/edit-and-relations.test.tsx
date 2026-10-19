// M4 renderer behavior: relation pickers and edit forms (PATCH + If-Match).

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { OperationForm } from "../src/OperationForm";
import { decodeDescriptor } from "../src/protocol";
import type { Reply, Transport } from "../src/transport";
import { publishedAdminDescriptor, publishedDescriptor, savedLesson, frameworkProblem } from "./support";

afterEach(cleanup);

const ok = (body: unknown, status = 200, etag?: string): Reply => ({ kind: "response", status, body: frameworkProblem(body, status), bytes: 1, etag });
const UNIT_A = "0199aaaa-0000-7000-8000-00000000000a";
const UNIT_B = "0199aaaa-0000-7000-8000-00000000000b";

function transport(descriptor: any, replies: Reply[] = []) {
  const submits: { input: Record<string, unknown>; record?: any; descriptor: any }[] = [];
  const lists: string[] = [];
  const t: Transport = {
    getManifest: async () => ok({}),
    getInteraction: async () => ok(descriptor),
    list: async (path, query) => {
      lists.push(`${path}?${new URLSearchParams(query).toString()}`);
      return ok({ items: [{ id: UNIT_A, name: "Number sense" }, { id: UNIT_B, name: "Geometry" }], next_cursor: null });
    },
    send: async () => ({ status: 500, body: null }),
    submit: async (d, input, record) => {
      submits.push({ input, record, descriptor: d });
      return replies.shift() ?? ok({ id: "x" }, 201);
    },
  };
  return { t, submits, lists };
}

describe("relation picker", () => {
  it("offers only what the scoped list operation returns and submits the chosen ID", async () => {
    const { t, submits, lists } = transport(publishedDescriptor());
    render(<OperationForm interaction="lessons.create.form" transport={t} />);
    const unit = await screen.findByLabelText("Unit (optional)");
    await waitFor(() => expect(unit.querySelectorAll("option")).toHaveLength(3));
    expect(lists).toEqual(["/api/v1/units?limit=100"]);
    expect([...unit.querySelectorAll("option")].map((o) => o.textContent)).toEqual(["None", "Number sense", "Geometry"]);

    await userEvent.type(await screen.findByLabelText("Title"), "Fractions");
    await userEvent.type(screen.getByLabelText("Grade level"), "4");
    await userEvent.selectOptions(unit, UNIT_B);
    await userEvent.click(screen.getByRole("button", { name: "Create lesson" }));
    await waitFor(() => expect(submits).toHaveLength(1));
    expect(submits[0]!.input).toEqual({ title: "Fractions", grade_level: 4, unit_id: UNIT_B });
  });
});

describe("edit forms", () => {
  const record = {
    path: { id: "01a0dc49-2bf1-7112-b58a-ea138be5bdf1" },
    etag: '"lessons.view:3"',
    values: { title: "Fractions", grade_level: 4, subtitle: "Parts", unit_id: UNIT_A, status: "draft", version: "3" },
  };

  it("sends only changed fields, the path ID and If-Match; clearing is null", async () => {
    const saved = ok(savedLesson({ ...record.values, subtitle: null, grade_level: 5, version: "4" }), 200, '"lessons.view:4"');
    const { t, submits } = transport(publishedAdminDescriptor(), [saved, ok(savedLesson({ version: "5" }), 200, '"lessons.view:5"')]);
    const onSuccess = vi.fn();
    render(<OperationForm interaction="lessons.admin_edit.form" transport={t} record={record} onSuccess={onSuccess} />);
    const grade = await screen.findByLabelText("Grade level (optional)");
    expect((grade as HTMLInputElement).value).toBe("4");
    await userEvent.clear(grade);
    await userEvent.type(grade, "5");
    await userEvent.clear(screen.getByLabelText("Subtitle (optional)"));
    await userEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(submits).toHaveLength(1));
    expect(submits[0]!.input).toEqual({
      id: record.path.id,
      expected_version: "3",
      grade_level: 5,
      subtitle: null,
    });
    expect(submits[0]!.record).toEqual({ path: record.path, etag: '"lessons.view:3"' });
    expect(onSuccess).toHaveBeenCalledWith(expect.anything(), expect.anything(), '"lessons.view:4"');

    // The next edit is guarded by the new ETag and sends only its own change.
    await userEvent.clear(screen.getByLabelText("Title (optional)"));
    await userEvent.type(screen.getByLabelText("Title (optional)"), "Fractions II");
    await userEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(submits).toHaveLength(2));
    expect(submits[1]!.input).toEqual({ id: record.path.id, expected_version: "4", title: "Fractions II" });
    expect(submits[1]!.record.etag).toBe('"lessons.view:4"');
  });

  it("explains a stale edit", async () => {
    const stale = ok({ status: 412, code: "precondition_failed" }, 412);
    const { t } = transport(publishedAdminDescriptor(), [stale]);
    render(<OperationForm interaction="lessons.admin_edit.form" transport={t} record={record} />);
    await userEvent.type(await screen.findByLabelText("Title (optional)"), "!");
    await userEvent.click(screen.getByRole("button", { name: "Save changes" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Someone changed this since you opened it");
  });
});

describe("decoder: M4 bindings", () => {
  it("keeps relation sources and templates on the API origin", () => {
    const d = publishedDescriptor();
    const relation = d.presentation.layout.children.find((c: any) => c.widget === "relation");
    relation.options.path = "https://evil.example/units";
    const result = decodeDescriptor(d);
    expect(result.ok).toBe(false);

    const edit = publishedAdminDescriptor();
    edit.operation.binding.pathParams = ["school_id"];
    expect(decodeDescriptor(edit).ok).toBe(false);
    const noField = publishedAdminDescriptor();
    noField.operation.binding.precondition.field = "title";
    expect(decodeDescriptor(noField).ok).toBe(false);
    const good = decodeDescriptor(publishedAdminDescriptor());
    expect(good.ok && good.descriptor.operation.precondition).toEqual({ field: "expected_version", etagOf: "lessons.view" });
  });
});
