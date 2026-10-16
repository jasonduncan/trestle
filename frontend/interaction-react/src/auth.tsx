// Sign-in, set-password and change-password forms for password accounts.
// Plain, accessible forms: labelled inputs, errors announced and linked to
// their fields. Passwords are never stored or logged.

import { type FormEvent, useEffect, useId, useRef, useState } from "react";

import {
  type FieldProblem,
  type SignInMethods,
  changePassword,
  inspectLink,
  forgetLinkToken,
  linkToken,
  loginUrl,
  passwordLogin,
  setPassword,
} from "./session";

interface Field {
  name: string;
  label: string;
  type: "email" | "password";
  autoComplete: string;
  value: string;
  onChange: (value: string) => void;
  error?: string;
  help?: string;
}

function TextField({ field }: { field: Field }) {
  const id = useId();
  const described = [field.help ? `${id}-help` : null, field.error ? `${id}-error` : null].filter(Boolean).join(" ");
  return (
    <div className="trestle-field">
      <label htmlFor={id}>{field.label}</label>
      {field.help && (
        <p className="trestle-help" id={`${id}-help`}>
          {field.help}
        </p>
      )}
      <input
        id={id}
        name={field.name}
        type={field.type}
        autoComplete={field.autoComplete}
        value={field.value}
        required
        aria-invalid={field.error ? true : undefined}
        aria-describedby={described || undefined}
        onChange={(e) => field.onChange(e.target.value)}
      />
      {field.error && (
        <p className="trestle-error" id={`${id}-error`}>
          {field.error}
        </p>
      )}
    </div>
  );
}

function Summary({ messages }: { messages: string[] }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (messages.length > 0) ref.current?.focus();
  }, [messages]);
  if (messages.length === 0) return null;
  return (
    <div className="trestle-summary" role="alert" tabIndex={-1} ref={ref}>
      {messages.map((m, i) => (
        <p key={i}>{m}</p>
      ))}
    </div>
  );
}

const byPointer = (errors: FieldProblem[], pointer: string) =>
  errors
    .filter((e) => e.pointer === pointer)
    .map((e) => e.message)
    .join(" ") || undefined;

export interface SignInProps {
  methods: SignInMethods;
  /** Called after a successful sign-in; typically reloads the session. */
  onSignedIn: () => void;
  /** Where OIDC sign-in returns to (a path on this site). */
  returnTo?: string;
}

export function SignIn({ methods, onSignedIn, returnTo = "/" }: SignInProps) {
  const headingId = useId();
  const [email, setEmail] = useState("");
  const [password, setPasswordValue] = useState("");
  const [messages, setMessages] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    const result = await passwordLogin(email, password);
    setBusy(false);
    if (result.ok) {
      setPasswordValue("");
      onSignedIn();
    } else {
      setMessages([result.message]);
    }
  };

  return (
    <section aria-labelledby={headingId} className="trestle-form">
      <h2 id={headingId}>Sign in</h2>
      {methods.password && (
        <form onSubmit={(e) => void submit(e)} noValidate>
          <Summary messages={messages} />
          <TextField field={{ name: "email", label: "Email", type: "email", autoComplete: "username", value: email, onChange: setEmail }} />
          <TextField
            field={{ name: "password", label: "Password", type: "password", autoComplete: "current-password", value: password, onChange: setPasswordValue }}
          />
          <button type="submit" disabled={busy}>
            Sign in
          </button>
          <p className="trestle-help">Forgot your password? Ask your administrator for a reset link.</p>
        </form>
      )}
      {methods.oidc && (
        <p>
          <a className="button" href={loginUrl(returnTo)}>
            Sign in with your organization
          </a>
        </p>
      )}
    </section>
  );
}

export interface SetPasswordProps {
  /** Called once the password is set and the browser is signed in. */
  onDone: () => void;
}

/** The page an invite or reset link opens (`/set-password#token=…`). */
export function SetPassword({ onDone }: SetPasswordProps) {
  const headingId = useId();
  // Read without side effects (StrictMode may run this twice), then take
  // the secret out of the address bar and history.
  const [token] = useState(() => linkToken());
  useEffect(() => forgetLinkToken(), []);
  const [link, setLink] = useState<{ email: string; purpose: "invite" | "reset" } | "invalid" | null>(null);
  const [password, setPasswordValue] = useState("");
  const [confirm, setConfirm] = useState("");
  const [errors, setErrors] = useState<FieldProblem[]>([]);
  const [messages, setMessages] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!token) {
      setLink("invalid");
      return;
    }
    void inspectLink(token).then((result) => setLink(result.ok ? result.value : "invalid"));
  }, [token]);

  if (link === null) return <p>Checking your link…</p>;
  if (link === "invalid" || !token) {
    return (
      <section aria-labelledby={headingId}>
        <h2 id={headingId}>This link can't be used</h2>
        <p>It may have been used already, replaced by a newer link, or expired. Ask your administrator for a new one.</p>
      </section>
    );
  }

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (password !== confirm) {
      setErrors([{ pointer: "/confirm", code: "mismatch", message: "The two passwords don't match." }]);
      setMessages(["The two passwords don't match."]);
      return;
    }
    setBusy(true);
    const result = await setPassword(token, password);
    setBusy(false);
    if (result.ok) {
      onDone();
      return;
    }
    setErrors(result.errors);
    setMessages(result.errors.length > 0 ? result.errors.map((err) => err.message) : [result.message]);
  };

  return (
    <section aria-labelledby={headingId} className="trestle-form">
      <h2 id={headingId}>{link.purpose === "invite" ? "Choose your password" : "Choose a new password"}</h2>
      <p>
        For <strong>{link.email}</strong>.
      </p>
      <form onSubmit={(e) => void submit(e)} noValidate>
        <Summary messages={messages} />
        <TextField
          field={{
            name: "password",
            label: "New password",
            type: "password",
            autoComplete: "new-password",
            value: password,
            onChange: setPasswordValue,
            help: "At least 10 characters. A few unrelated words work well.",
            error: byPointer(errors, "/password"),
          }}
        />
        <TextField
          field={{
            name: "confirm",
            label: "Confirm password",
            type: "password",
            autoComplete: "new-password",
            value: confirm,
            onChange: setConfirm,
            error: byPointer(errors, "/confirm"),
          }}
        />
        <button type="submit" disabled={busy}>
          Set password
        </button>
      </form>
    </section>
  );
}

export interface ChangePasswordProps {
  csrfToken: string;
  onDone: () => void;
}

export function ChangePassword({ csrfToken, onDone }: ChangePasswordProps) {
  const headingId = useId();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [errors, setErrors] = useState<FieldProblem[]>([]);
  const [messages, setMessages] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    const result = await changePassword(csrfToken, current, next);
    setBusy(false);
    if (result.ok) {
      setCurrent("");
      setNext("");
      onDone();
      return;
    }
    setErrors(result.errors);
    setMessages(result.errors.length > 0 ? result.errors.map((err) => err.message) : [result.message]);
  };

  return (
    <section aria-labelledby={headingId} className="trestle-form">
      <h2 id={headingId}>Change password</h2>
      <form onSubmit={(e) => void submit(e)} noValidate>
        <Summary messages={messages} />
        <TextField
          field={{
            name: "current_password",
            label: "Current password",
            type: "password",
            autoComplete: "current-password",
            value: current,
            onChange: setCurrent,
            error: byPointer(errors, "/current_password"),
          }}
        />
        <TextField
          field={{
            name: "new_password",
            label: "New password",
            type: "password",
            autoComplete: "new-password",
            value: next,
            onChange: setNext,
            help: "At least 10 characters. Other devices will be signed out.",
            error: byPointer(errors, "/new_password"),
          }}
        />
        <button type="submit" disabled={busy}>
          Change password
        </button>
      </form>
    </section>
  );
}
