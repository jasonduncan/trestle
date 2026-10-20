import React from "react";
import { createRoot } from "react-dom/client";
import { AdminHost } from "@trestle-framework/admin-react";
import "@trestle-framework/admin-react/styles.css";

createRoot(document.getElementById("root")!).render(<AdminHost title="{{name}}"
  tenantProfile={{ header: "Trestle-Tenant", label: "Workspace", path: "/api/v1/workspaces/mine" }} />);
