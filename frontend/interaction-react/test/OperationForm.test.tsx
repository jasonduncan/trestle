// OperationForm against the descriptor the backend actually publishes.

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { OperationForm } from "../src/OperationForm";
import type { Reply, Transport } from "../src/transport";
import { publishedDescriptor, repoJson, savedLesson, frameworkProblem } from "./support";

afterEach(cleanup);

const ok = (body: unknown, status = 200): Reply => ({
  kind: "response",
  status,
  body: frameworkProblem(body, status),
  bytes: JSON.stringify(body).length,
});

function withAvailability(descriptor: any, available = true, reason?: string) {
  descriptor.availability = reason ? { available, reason } : { available };
  return descriptor;
}

/** A future release: one more optional field and its presentation. */
function withNotes(descriptor: any) {
  descriptor.input.schema.properties.notes = { type: "string", maxLength: 500 };
  descriptor.presentation.layout.children.push({ kind: "field", pointer: "/notes", widget: "textarea", label: "Teacher notes" });
  descriptor.revision = "r-bbbbbbbbbbbbbbbb";
  return descriptor;
}

/** ADR-028: an optional tagged union, as the server exports it. */
function withPayment(descriptor: any) {
  descriptor.input.schema.properties.payment = structuredClone(repoJson("fixtures/contracts/unions.json").schema.properties.payment);
  descriptor.presentation.layout.children.push({ kind: "field", pointer: "/payment", widget: "union", label: "Payment" });
  descriptor.requiredFeatures.push("forms.union@1");
  return descriptor;
}

const UNITS = [
  { id: "0199aaaa-0000-7000-8000-000000000001", name: "Number sense", version: "1" },
  { id: "0199aaaa-0000-7000-8000-000000000002", name: "Geometry", version: "1" },
];

function fakeTransport(descriptors: any[], replies: Reply[] = []) {
  const loads: string[] = [];
  const lists: string[] = [];
  const submits: { descriptor: any; body: Record<string, unknown>; record?: any }[] = [];
  const transport: Transport = {
    getManifest: async () => ok({}),
    list: async (path) => {
      lists.push(path);
      return ok({ items: UNITS, next_cursor: null });
    },
    send: async () => ({ status: 500, body: null }),
    getInteraction: async (id) => {
      loads.push(id);
      const next = descriptors.length > 1 ? descriptors.shift() : descriptors[0];
      return "kind" in next ? next : ok(next);
    },
    submit: async (descriptor, body, record) => {
      submits.push({ descriptor, body, record });
      return replies.shift() ?? ok(savedLesson(), 201);
    },
  };
  return { transport, loads, lists, submits };
}

async function fill(label: RegExp, text: string) {
  const input = await screen.findByLabelText(label);
  await userEvent.clear(input);
  if (text) await userEvent.type(input, text);
}

describe("OperationForm", () => {
  it("groups the same contract fields into labeled sections and columns", async () => {
    const descriptor = publishedDescriptor();
    const children = descriptor.presentation.layout.children;
    descriptor.protocolVersion = "1.1";
    descriptor.requiredFeatures.push("forms.layout@1");
    descriptor.presentation.layout = { kind: "section", id: "details", label: "Lesson details", children: [{ kind: "columns", id: "main", children: [
      { kind: "section", id: "left", label: "Core fields", children: children.slice(0, 2) },
      { kind: "section", id: "right", label: "More fields", children: children.slice(2) },
    ] }] };
    const { transport } = fakeTransport([descriptor]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect(await screen.findByRole("group", { name: "Lesson details" })).toBeDefined();
    expect(screen.getByLabelText("Title")).toBeDefined();
    expect(screen.getByLabelText("Grade level")).toBeDefined();
  });
  it("renders the published form with labels and help", async () => {
    const { transport } = fakeTransport([withAvailability(publishedDescriptor())]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect(await screen.findByRole("heading", { name: "Create lesson" })).toBeTruthy();
    const grade = screen.getByLabelText("Grade level");
    const help = screen.getByText("1 to 12.");
    expect(grade.getAttribute("aria-describedby")).toContain(help.id);
    expect(screen.getByLabelText("Title").getAttribute("aria-required")).toBe("true");
  });

  it("validates locally from the schema and does not submit invalid input", async () => {
    const { transport, submits } = fakeTransport([publishedDescriptor()]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Grade level/, "13");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    expect(submits).toHaveLength(0);
    const summary = await screen.findByRole("alert");
    expect(summary.textContent).toContain("Title: This field cannot be empty.");
    expect(summary.textContent).toContain("Grade level: Must be at most 12.");
    expect(document.activeElement).toBe(summary);
    expect(screen.getByLabelText("Grade level").getAttribute("aria-invalid")).toBe("true");
  });

  it("submits through the binding with its revision and clears on success", async () => {
    const onSuccess = vi.fn();
    const { transport, submits } = fakeTransport([publishedDescriptor()]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} onSuccess={onSuccess} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    await waitFor(() => expect(onSuccess).toHaveBeenCalledOnce());
    expect(submits[0]!.body).toEqual({ title: "Fractions", grade_level: 4 });
    expect(submits[0]!.descriptor.revision).toBe(publishedDescriptor().revision);
    expect((screen.getByLabelText("Title") as HTMLInputElement).value).toBe("");
    expect(screen.getByText("Saved.")).toBeTruthy();
  });

  it("renders a newly published optional field with no code change", async () => {
    const { transport, submits } = fakeTransport([withNotes(publishedDescriptor())]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect(await screen.findByLabelText("Teacher notes (optional)")).toBeTruthy();
    await fill(/Title/, "Ratios");
    await fill(/Grade level/, "6");
    await userEvent.click(screen.getByRole("button", { name: "Create lesson" }));
    await waitFor(() => expect(submits).toHaveLength(1));
    expect(submits[0]!.body).toEqual({ title: "Ratios", grade_level: 6 });
    await fill(/Title/, "Ratios 2");
    await fill(/Grade level/, "6");
    await fill(/Teacher notes/, "Use the bar model");
    await userEvent.click(screen.getByRole("button", { name: "Create lesson" }));
    await waitFor(() => expect(submits).toHaveLength(2));
    expect(submits[1]!.body).toEqual({ title: "Ratios 2", grade_level: 6, notes: "Use the bar model" });
  });

  it("maps server field errors onto fields", async () => {
    const conflict = ok(
      {
        status: 422,
        code: "validation_failed",
        errors: [{ pointer: "/title", code: "already_exists", message: "A lesson with this title already exists in this school." }],
      },
      422,
    );
    const { transport } = fakeTransport([publishedDescriptor()], [conflict]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    const title = screen.getByLabelText("Title");
    await waitFor(() => expect(title.getAttribute("aria-invalid")).toBe("true"));
    expect(document.getElementById(title.getAttribute("aria-describedby")!)?.textContent).toContain("already exists");
    expect((title as HTMLInputElement).value).toBe("Fractions");
  });

  it("switches a union's fields by variant and shows member errors on the union", async () => {
    const rejected = ok(
      { status: 422, code: "validation_failed", errors: [{ pointer: "/payment/last4", code: "card_declined", message: "This card was declined." }] },
      422,
    );
    const { transport, submits } = fakeTransport([withPayment(publishedDescriptor())], [rejected]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    const payment = await screen.findByLabelText(/Payment/);
    expect(screen.queryByLabelText(/Last4/)).toBeNull();
    await userEvent.selectOptions(payment, "invoice");
    expect(await screen.findByLabelText(/Due/)).toBeTruthy();
    await userEvent.selectOptions(payment, "card");
    expect(screen.queryByLabelText(/Due/)).toBeNull();
    await fill(/Last4/, "42");
    await userEvent.click(screen.getByRole("button", { name: "Create lesson" }));
    expect(submits).toHaveLength(0);
    expect((await screen.findByRole("alert")).textContent).toContain("Payment: Last4: Use at least 4 characters.");
    expect(payment.getAttribute("aria-invalid")).toBe("true");

    await fill(/Last4/, "4242");
    await userEvent.click(screen.getByRole("button", { name: "Create lesson" }));
    await waitFor(() => expect(submits).toHaveLength(1));
    expect(submits[0]!.body).toEqual({ title: "Fractions", grade_level: 4, payment: { method: "card", last4: "4242" } });
    await waitFor(() => expect(document.getElementById(payment.getAttribute("aria-describedby")!)?.textContent).toBe("Last4: This card was declined."));
  });

  it("reloads a stale form, keeps entries and never resubmits by itself", async () => {
    const stale = ok({ status: 409, code: "interaction_stale", detail: "This form changed." }, 409);
    const { transport, submits, loads } = fakeTransport(
      [publishedDescriptor(), withNotes(publishedDescriptor())],
      [stale],
    );
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    expect(await screen.findByText(/This form changed since you opened it/)).toBeTruthy();
    expect(loads).toHaveLength(2);
    expect(submits).toHaveLength(1);
    expect((screen.getByLabelText("Title") as HTMLInputElement).value).toBe("Fractions");
    expect(screen.getByLabelText("Teacher notes (optional)")).toBeTruthy();
  });

  it("explains a permission revoked after the form loaded", async () => {
    const denied = ok({ status: 403, code: "permission_denied", title: "Forbidden" }, 403);
    const { transport } = fakeTransport([withAvailability(publishedDescriptor())], [denied]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    expect((await screen.findByRole("alert")).textContent).toContain("You don't have permission to do this.");
  });

  it("reports an unknown outcome on network failure and does not retry", async () => {
    const { transport, submits } = fakeTransport([publishedDescriptor()], [{ kind: "network-error", message: "timeout" }]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await fill(/Title/, "Fractions");
    await fill(/Grade level/, "4");
    await userEvent.click(await screen.findByRole("button", { name: "Create lesson" }));
    expect((await screen.findByRole("alert")).textContent).toContain("couldn't confirm whether this was saved");
    expect(submits).toHaveLength(1);
  });

  it("blocks submission when a required feature or essential widget is missing", async () => {
    const feature = publishedDescriptor();
    feature.requiredFeatures.push("forms.wizard@1");
    const { transport } = fakeTransport([feature]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect((await screen.findByRole("alert")).textContent).toContain("needs renderer feature forms.wizard@1");
    expect((screen.getByRole("button", { name: "Create lesson" }) as HTMLButtonElement).disabled).toBe(true);
    cleanup();

    const widget = publishedDescriptor();
    widget.presentation.layout.children[0].widget = "rich-text";
    const second = fakeTransport([widget]);
    render(<OperationForm interaction="lessons.create.form" transport={second.transport} />);
    expect((await screen.findByRole("alert")).textContent).toContain('field "Title" needs control rich-text');
    cleanup();

    // An optional field with an unknown widget is simply left out.
    const optional = withNotes(publishedDescriptor());
    optional.presentation.layout.children.at(-1).widget = "rich-text";
    const third = fakeTransport([optional]);
    render(<OperationForm interaction="lessons.create.form" transport={third.transport} />);
    expect(await screen.findByLabelText("Title")).toBeTruthy();
    expect(screen.queryByLabelText(/Teacher notes/)).toBeNull();
    expect((screen.getByRole("button", { name: "Create lesson" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("shows advisory unavailability without granting or hiding authority", async () => {
    const { transport } = fakeTransport([withAvailability(publishedDescriptor(), false, "permission_denied")]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect((await screen.findByRole("note")).textContent).toContain("You don't have permission to do this.");
    expect((screen.getByRole("button", { name: "Create lesson" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("renders labels and help as text, never markup", async () => {
    const hostile = publishedDescriptor();
    hostile.presentation.layout.children[0].label = '<img src=x onerror="alert(1)">Title';
    hostile.presentation.layout.children[1].help = "<script>alert(2)</script>";
    const { transport } = fakeTransport([hostile]);
    const { container } = render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    await screen.findByRole("heading");
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("script")).toBeNull();
    expect(container.textContent).toContain('<img src=x onerror="alert(1)">Title');
  });

  it("shows why a form cannot be loaded", async () => {
    const { transport } = fakeTransport([ok({ status: 403, code: "tenant_not_permitted" }, 403)]);
    render(<OperationForm interaction="lessons.create.form" transport={transport} />);
    expect((await screen.findByRole("alert")).textContent).toBe("You're not a member of the selected tenant.");
  });
});
