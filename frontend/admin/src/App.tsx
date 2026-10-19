import { AdminHost, type TenantProfile } from "@trestle/admin-react";
import "@trestle/admin-react/styles.css";
function parse<T>(value: string | undefined): T | undefined { try { return value ? JSON.parse(value) as T : undefined; } catch { return undefined; } }
const tenant = parse<TenantProfile>(import.meta.env.VITE_ADMIN_TENANT);
const users = import.meta.env.DEV ? parse<Record<string, { token: string }>>(import.meta.env.VITE_ADMIN_DEV_USERS) : undefined;
export function App() { return <AdminHost title={import.meta.env.VITE_ADMIN_TITLE ?? "Trestle"} tenantProfile={tenant ? { ...tenant, dev: import.meta.env.DEV ? tenant.dev : undefined } : undefined} developmentUsers={users} />; }
