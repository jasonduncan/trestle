-- lessons.0001_initial
CREATE TABLE lessons (
    id          uuid        PRIMARY KEY,
    school_id   uuid        NOT NULL REFERENCES schools (id),
    title       text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    grade_level smallint    NOT NULL CHECK (grade_level BETWEEN 1 AND 12),
    status      text        NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published')),
    version     bigint      NOT NULL DEFAULT 1 CHECK (version >= 1),
    created_by  uuid        NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT lessons_school_title_key UNIQUE (school_id, title)
);
