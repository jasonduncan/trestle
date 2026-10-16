-- audit.0001_initial: facts about successful state changes, written in the
-- same transaction as the change (design doc §10.3). No request bodies.
CREATE TABLE audit_events (
    id                uuid        PRIMARY KEY,
    occurred_at       timestamptz NOT NULL DEFAULT now(),
    actor_id          uuid        NOT NULL,
    tenant_id         uuid,
    operation         text        NOT NULL,
    operation_version text        NOT NULL,
    target_type       text        NOT NULL,
    target_id         text        NOT NULL,
    request_id        text
);

CREATE INDEX audit_events_tenant_idx ON audit_events (tenant_id, occurred_at);
