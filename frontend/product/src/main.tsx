import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { SetPassword } from "@trestle/interaction-react";

import { App } from "./App";
import "./styles.css";

// Invite and reset links open /set-password#token=…; everything else is the app.
const page =
  window.location.pathname === "/set-password" ? (
    <div className="app">
      <header>
        <h1>Trestle Pilot</h1>
      </header>
      <main className="form-panel">
        <SetPassword onDone={() => window.location.assign("/")} />
      </main>
    </div>
  ) : (
    <App />
  );

createRoot(document.getElementById("root")!).render(<StrictMode>{page}</StrictMode>);
