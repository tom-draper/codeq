CREATE TYPE colour AS ENUM ('red', 'green');

CREATE TABLE shapes (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

-- A circle belongs to a shape.
CREATE TABLE circles (
    id       INTEGER PRIMARY KEY,
    shape_id INTEGER REFERENCES shapes (id),
    radius   REAL DEFAULT 1.0,
    label    TEXT DEFAULT 'circle'
);

CREATE INDEX circles_by_shape ON circles (shape_id);

CREATE VIEW big_circles AS
SELECT id, radius FROM circles WHERE radius > 10;

CREATE FUNCTION touch_updated_at() RETURNS trigger AS $$
BEGIN
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER circles_touch BEFORE UPDATE ON circles
FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
