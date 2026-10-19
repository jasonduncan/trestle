# Admin host

This separate frontend uses the registered Notes admin, browser sign-in and the
authorized `/api/v1/workspaces/mine` operation. Create an account and grant it the
`member` role using the project's operator commands. Membership never substitutes
for command authorization. No development tokens are embedded in this bundle.

The frontend packages are local release candidates until published. In the Trestle
checkout run `cd frontend && npm ci && npm run build:packages`, then `npm pack` in
`client`, `interaction-react`, and `admin-react`. Install all three resulting
tarballs here using `npm install /path/to/trestle-client-*.tgz ...`, then `npm run dev`.
After package publication a normal `npm install` resolves the declared version.

Run the API on localhost:8000 and set its public origin to `http://localhost:5174`
so cookie and CSRF checks match the frontend. `npm run build` emits static assets;
deploy them behind the same origin as `/api`, `/_meta`, and `/auth`. The backend
continues to serve JSON. Use a private, no-store policy for API and metadata routes.

The starter directory supports up to 100 memberships and refuses larger results;
add an authorized paged directory if the application needs more. Its SQL uses the
starter's fixed workspace schema; review it when changing tenancy.
