import { useCallback, useEffect, useMemo, useState } from "react";

import {
  ChangePassword,
  OperationForm,
  SignIn,
  createTransport,
  loadMethods,
  sessionHeaders,
  useSession,
  type Descriptor,
  type SignInMethods,
} from "@trestle/interaction-react";

import * as api from "./generated/api";

// Identity comes from the server: with browser login configured, a session
// cookie plus an in-memory CSRF token (§12.2); otherwise the development
// tokens below (matching `pilot seed-dev`), held in memory only.
const USERS = {
  alice: { token: "alice-dev-token", note: "teacher at Northside" },
  bob: { token: "bob-dev-token", note: "teacher at Southside" },
  carol: { token: "carol-dev-token", note: "viewer at Northside" },
  olivia: { token: "olivia-dev-token", note: "owner at Northside" },
} as const;
type User = keyof typeof USERS;

const DEV_SCHOOLS = [
  { id: "11111111-1111-4111-8111-111111111111", name: "Northside Elementary" },
  { id: "22222222-2222-4222-8222-222222222222", name: "Southside Middle" },
];

interface ManifestEntry {
  interactionId: string;
  title: string;
  revision: string;
  availability: { available: boolean; reason?: string };
}

type Row = Record<string, unknown>;

function problemText(problem: api.Problem): string {
  switch (problem.code) {
    case "stale_version":
      return "Someone changed this lesson; the list has been refreshed.";
    case "invalid_transition":
      return "Only draft lessons can be published.";
    case "permission_denied":
      return "You don't have permission to do this.";
    default:
      return problem.detail ?? problem.title ?? `Request failed (${problem.status}).`;
  }
}

export function App() {
  const session = useSession();
  const refreshSession = session.refresh;
  const signedIn = session.state.kind === "signed-in";
  const [methods, setMethods] = useState<SignInMethods | null>(null);
  const [changing, setChanging] = useState(false);

  useEffect(() => {
    if (session.state.kind === "signed-out" || session.state.kind === "signed-in") void loadMethods().then(setMethods);
  }, [session.state.kind]);
  const [user, setUser] = useState<User>("alice");
  const [schools, setSchools] = useState<{ id: string; name: string }[]>(DEV_SCHOOLS);
  const [school, setSchool] = useState(DEV_SCHOOLS[0]!.id);
  const [manifest, setManifest] = useState<ManifestEntry[] | string>("Loading…");
  const [rows, setRows] = useState<Row[]>([]);
  const [columns, setColumns] = useState<string[]>([]);
  const [lessons, setLessons] = useState<api.LessonsView[]>([]);
  const [units, setUnits] = useState<api.UnitsView[]>([]);
  const [message, setMessage] = useState("");
  const [unitName, setUnitName] = useState("");
  const [formKey, setFormKey] = useState(0);

  const transport = useMemo(
    () =>
      createTransport({
        headers: () => ({
          ...(signedIn ? sessionHeaders(session.state) : { Authorization: `Bearer ${USERS[user].token}` }),
          "Trestle-Tenant": school,
        }),
      }),
    [signedIn, session.state, user, school],
  );

  // Signed in: the schools come from the caller's memberships.
  useEffect(() => {
    if (!signedIn) return;
    void api.schoolsMine(transport, {}).then((result) => {
      if (!result.ok) return;
      const mine = result.value.items.map((m) => ({ id: m.school, name: `${m.name} (${m.role})` }));
      setSchools(mine);
      setSchool((current) => (mine.some((s) => s.id === current) ? current : (mine[0]?.id ?? "")));
    });
  }, [signedIn, transport]);

  // The custom screens use the generated client; the create form uses metadata.
  const refresh = useCallback(async () => {
    const [lessonPage, unitPage] = await Promise.all([
      api.lessonsList(transport, { limit: 50 }),
      api.unitsList(transport, { limit: 100 }),
    ]);
    setLessons(lessonPage.ok ? lessonPage.value.items : []);
    setUnits(unitPage.ok ? unitPage.value.items : []);
  }, [transport]);

  useEffect(() => {
    if (session.state.kind === "loading" || session.state.kind === "signed-out") return;
    let cancelled = false;
    setManifest("Loading…");
    setMessage("");
    void transport.getManifest().then((reply) => {
      if (cancelled) return;
      if (reply.kind === "response" && reply.status === 200) {
        setManifest((reply.body as { interactions: ManifestEntry[] }).interactions);
      } else if (reply.kind === "response" && reply.status === 401 && signedIn) {
        void refreshSession(); // the session ended elsewhere
      } else {
        setManifest("No interactions are available for this user and school.");
      }
    });
    void refresh();
    return () => {
      cancelled = true;
    };
  }, [transport, refresh, refreshSession, signedIn, session.state.kind]);

  const created = (output: unknown, descriptor: Descriptor) => {
    // Columns come from the published output contract, so a newly added
    // field appears here without changing this code.
    setColumns(descriptor.output.schema.fields.map((f) => f.name));
    setRows((previous) => [output as Row, ...previous]);
    void refresh();
  };

  const publish = async (lesson: api.LessonsView) => {
    const result = await api.lessonsPublish(transport, { id: lesson.id, expected_version: lesson.version });
    setMessage(result.ok ? `Published “${result.value.title}”.` : problemText(result.problem));
    await refresh();
  };

  const addUnit = async () => {
    const result = await api.unitsCreate(transport, { name: unitName });
    if (result.ok) {
      setUnitName("");
      setMessage(`Added unit “${result.value.name}”.`);
      setFormKey((k) => k + 1);
    } else {
      setMessage(result.problem.errors?.[0]?.message ?? problemText(result.problem));
    }
    await refresh();
  };

  if (session.state.kind === "loading") {
    return <p className="app">Loading…</p>;
  }
  if (session.state.kind === "signed-out") {
    return (
      <div className="app">
        <header>
          <h1>Trestle Pilot</h1>
        </header>
        <main>
          {methods ? (
            <SignIn methods={methods} onSignedIn={() => void refreshSession()} returnTo={window.location.pathname + window.location.search} />
          ) : (
            <p>You are signed out.</p>
          )}
        </main>
      </div>
    );
  }

  const schoolPicker = (
    <label>
      School{" "}
      <select value={school} onChange={(e) => setSchool(e.target.value)}>
        {schools.map((s) => (
          <option key={s.id} value={s.id}>
            {s.name}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <div className="app">
      <header>
        <h1>Trestle Pilot</h1>
        {session.state.kind === "signed-in" ? (
          <fieldset className="identity">
            <legend>Signed in</legend>
            {schoolPicker}
            {methods?.password && (
              <button type="button" className="secondary" aria-expanded={changing} onClick={() => setChanging((open) => !open)}>
                Change password
              </button>
            )}
            <button type="button" onClick={() => void session.signOut()}>
              Sign out
            </button>
          </fieldset>
        ) : (
          <fieldset className="identity">
            <legend>Development sign-in</legend>
            <label>
              User{" "}
              <select value={user} onChange={(e) => setUser(e.target.value as User)}>
                {Object.entries(USERS).map(([name, { note }]) => (
                  <option key={name} value={name}>
                    {name} ({note})
                  </option>
                ))}
              </select>
            </label>
            {schoolPicker}
          </fieldset>
        )}
      </header>

      <main>
        <p className="message" role="status" aria-live="polite">
          {message}
        </p>

        {changing && session.state.kind === "signed-in" && (
          <section className="form-panel">
            <ChangePassword
              csrfToken={session.state.csrfToken}
              onDone={() => {
                setChanging(false);
                setMessage("Password changed. Your other devices were signed out.");
              }}
            />
          </section>
        )}

        <section aria-labelledby="available-heading">
          <h2 id="available-heading">Available to you</h2>
          {typeof manifest === "string" ? (
            <p>{manifest}</p>
          ) : (
            <ul className="manifest">
              {manifest.map((entry) => (
                <li key={entry.interactionId}>
                  {entry.title}: {entry.availability.available ? "available" : `unavailable (${entry.availability.reason})`}{" "}
                  <code>{entry.revision}</code>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section className="form-panel">
          <OperationForm key={`${signedIn ? "session" : user}:${school}:${formKey}`} interaction="lessons.create.form" transport={transport} onSuccess={created} />
        </section>

        <section aria-labelledby="lessons-heading">
          <h2 id="lessons-heading">Lessons</h2>
          {lessons.length === 0 ? (
            <p>No lessons yet.</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th scope="col">Title</th>
                  <th scope="col">Grade</th>
                  <th scope="col">Unit</th>
                  <th scope="col">Minutes</th>
                  <th scope="col">Status</th>
                  <th scope="col">Version</th>
                  <th scope="col">
                    <span className="visually-hidden">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {lessons.map((lesson) => (
                  <tr key={lesson.id}>
                    <td>{lesson.title}</td>
                    <td>{lesson.grade_level}</td>
                    <td>{units.find((u) => u.id === lesson.unit_id)?.name ?? "—"}</td>
                    <td>{lesson.duration_minutes ?? "—"}</td>
                    <td>{lesson.status}</td>
                    <td>{lesson.version}</td>
                    <td>
                      {lesson.status === "draft" && (
                        <button type="button" onClick={() => void publish(lesson)} aria-label={`Publish ${lesson.title}`}>
                          Publish
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>

        <section aria-labelledby="units-heading">
          <h2 id="units-heading">Units</h2>
          <p>{units.length === 0 ? "No units yet." : units.map((u) => u.name).join(", ")}</p>
          <form
            className="inline-form"
            onSubmit={(e) => {
              e.preventDefault();
              void addUnit();
            }}
          >
            <label htmlFor="unit-name">New unit</label>
            <input id="unit-name" value={unitName} onChange={(e) => setUnitName(e.target.value)} />
            <button type="submit">Add unit</button>
          </form>
        </section>

        {rows.length > 0 && (
          <section aria-labelledby="created-heading">
            <h2 id="created-heading">Created in this session</h2>
            <table>
              <thead>
                <tr>
                  {columns.map((c) => (
                    <th key={c} scope="col">
                      {c}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((row, i) => (
                  <tr key={String(row.id ?? i)}>
                    {columns.map((c) => (
                      <td key={c}>{row[c] === undefined || row[c] === null ? "—" : String(row[c])}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
        )}
      </main>
    </div>
  );
}
