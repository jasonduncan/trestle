import React from "react";
import { createRoot } from "react-dom/client";
import { AdminHost } from "@trestle/admin-react";
import "@trestle/admin-react/styles.css";

createRoot(document.getElementById("root")!).render(<AdminHost title="{{name}}"
  tenantProfile={{ header: "Trestle-Tenant", label: "Workspace", path: "/api/v1/workspaces/mine" }} />);
