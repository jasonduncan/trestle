-- schools.0001_initial: tenants and verified membership.
CREATE TABLE schools (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE school_memberships (
    school_id  uuid        NOT NULL REFERENCES schools (id),
    actor_id   uuid        NOT NULL,
    role       text        NOT NULL CHECK (role IN ('owner', 'teacher', 'viewer')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (school_id, actor_id)
);

CREATE INDEX school_memberships_actor_idx ON school_memberships (actor_id);
