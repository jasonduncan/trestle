-- lessons.0002_subtitle: optional subtitle (expand only). Nullable with no
-- default, so builds that predate it keep working against this schema.
ALTER TABLE lessons
    ADD COLUMN subtitle text
    CONSTRAINT lessons_subtitle_length CHECK (subtitle IS NULL OR char_length(subtitle) BETWEEN 1 AND 200);
