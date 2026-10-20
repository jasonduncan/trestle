# Local loop for Trestle. Needs a running PostgreSQL 13+ that your user can
# reach (set DATABASE_URL / TRESTLE_TEST_DATABASE_URL if it is not local).
DATABASE_URL ?= postgres://localhost/trestle_dev
export DATABASE_URL

.PHONY: db migrate plan seed run registry snapshots test lint check frontend frontend-test web admin demo-m2 demo-m4 demo-m5 demo-accounts drill-tls drill-restore drill-edge load audit

db: ## Check PostgreSQL is running and create trestle_dev
	@pg_isready -q || { echo "PostgreSQL is not running; start it (any 13+) and retry"; exit 1; }
	@psql -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname = 'trestle_dev'" | grep -q 1 || createdb trestle_dev

migrate: ## Apply committed migrations (migration-only binary)
	cargo run -q -p pilot-migrations --bin pilot-migrate

plan: ## Show migration plan without changing anything
	cargo run -q -p pilot-migrations --bin pilot-migrate -- --plan

seed: ## Insert development schools and memberships
	cargo run -q -p pilot -- seed-dev

run: ## Serve the pilot API on TRESTLE_BIND (default 127.0.0.1:8000)
	cargo run -p pilot -- serve

registry: ## Assemble and validate the registry (offline)
	cargo run -q -p pilot -- check

snapshots: ## Regenerate examples/pilot/published/*.json (and append to revisions.json) after an intended change
	UPDATE_SNAPSHOTS=1 cargo test -q -p pilot --test published -- --test-threads=1

test: ## All tests; integration tests create and drop trestle_test_* databases
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

frontend: ## Install frontend dependencies (npm workspace in frontend/)
	cd frontend && npm install

frontend-test: ## Renderer tests (fixtures shared with the server) and typechecks
	cd frontend && npm test && npm run typecheck

web: ## React product app on http://127.0.0.1:5173, proxying /api and /_meta to the API
	cd frontend && npm run dev --workspace product

admin: ## Admin shell on http://127.0.0.1:5174/admin/ (owner: olivia), proxying to the API
	cd frontend && npm run dev --workspace admin

demo-m2: ## Real-browser M2 demonstration across releases A and B (writes docs/evidence/)
	cd frontend && npm run e2e

demo-m4: ## Real-browser M4 demonstration: product + admin on the same operations
	cd frontend && node e2e/run-m4.mjs

demo-m5: ## Real-browser M5 demonstration: rolling additive change with the M4 release, OIDC sessions, CSRF
	cd frontend && node e2e/run-m5.mjs

demo-accounts: ## Real-browser password accounts: invite by email, set, change, reset, admin, lockout
	cd frontend && node e2e/run-accounts.mjs

drill-tls: ## Database TLS drill on a throwaway PostgreSQL (verify-full; refusals)
	scripts/drills/db-tls.sh

drill-restore: ## Backup, restore into a new database, forward repair
	scripts/drills/restore.sh

drill-edge: ## deploy/Caddyfile in a real Caddy before two production replicas (needs caddy)
	scripts/drills/edge.sh

audit: ## Dependency advisories: cargo audit (ignores documented in .cargo/audit.toml) and npm audit
	cargo audit --deny warnings
	cd frontend && npm audit

load: ## Load comparison: Trestle vs hand-written Axum baseline (release builds, ~12 min)
	scripts/load/compare.sh

check: lint test frontend-test
