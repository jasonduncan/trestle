import { AdminHost, type TenantProfile } from "@trestle-framework/admin-react";
import "@trestle-framework/admin-react/styles.css";
function parse<T>(value: string | undefined): T | undefined { try { return value ? JSON.parse(value) as T : undefined; } catch { return undefined; } }
const tenant = parse<TenantProfile>(import.meta.env.VITE_ADMIN_TENANT);
const users = import.meta.env.DEV ? parse<Record<string, { token: string }>>(import.meta.env.VITE_ADMIN_DEV_USERS) : undefined;
const profile = import.meta.env.VITE_ADMIN_PROFILE === "product" ? "product" : "admin";
export function App() { return <AdminHost title={import.meta.env.VITE_ADMIN_TITLE ?? "Trestle"} tenantProfile={tenant ? { ...tenant, dev: import.meta.env.DEV ? tenant.dev : undefined } : undefined} developmentUsers={users} profile={profile} />; }
