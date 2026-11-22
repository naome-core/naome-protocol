CREATE TABLE IF NOT EXISTS rep_search_schema(version INTEGER PRIMARY KEY CHECK(version=1));
INSERT OR IGNORE INTO rep_search_schema VALUES(1);
CREATE TABLE IF NOT EXISTS rep_search_revision(value INTEGER NOT NULL);
INSERT INTO rep_search_revision SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM rep_search_revision);
CREATE TABLE IF NOT EXISTS rep_search_heads(
    head TEXT PRIMARY KEY,
    cursor TEXT,
    dirty INTEGER NOT NULL DEFAULT 0,
    finished INTEGER NOT NULL DEFAULT 0,
    limited INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS rep_search_waiting(
    source TEXT NOT NULL,
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    PRIMARY KEY(source, head)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rep_search_waiting_head ON rep_search_waiting(head);
CREATE TABLE IF NOT EXISTS rep_search_entries(
    id INTEGER PRIMARY KEY,
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    object TEXT NOT NULL,
    kind INTEGER NOT NULL,
    subject TEXT NOT NULL,
    scope TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS rep_search_entries_head ON rep_search_entries(head);
CREATE TABLE IF NOT EXISTS rep_search_sources(
    source TEXT NOT NULL,
    entry INTEGER NOT NULL REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    PRIMARY KEY(source, entry)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rep_search_sources_entry ON rep_search_sources(entry);
CREATE TABLE IF NOT EXISTS rep_search_passages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    head TEXT NOT NULL,
    entry INTEGER NOT NULL REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    location TEXT NOT NULL,
    text TEXT NOT NULL,
    normalized TEXT NOT NULL,
    priority INTEGER NOT NULL,
    semantic INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS rep_search_passages_semantic ON rep_search_passages(head,id) WHERE semantic;
CREATE INDEX IF NOT EXISTS rep_search_passages_entry ON rep_search_passages(entry);
CREATE TABLE IF NOT EXISTS rep_search_terms(
    head TEXT NOT NULL,
    term TEXT NOT NULL,
    passage INTEGER NOT NULL REFERENCES rep_search_passages(id) ON DELETE CASCADE,
    PRIMARY KEY(head, term, passage)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rep_search_terms_passage ON rep_search_terms(passage);
CREATE TABLE IF NOT EXISTS rep_search_vector_progress(
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    model TEXT NOT NULL,
    cursor INTEGER NOT NULL,
    PRIMARY KEY(head,model)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rep_search_vectors(
    head TEXT NOT NULL,
    model TEXT NOT NULL,
    passage INTEGER NOT NULL REFERENCES rep_search_passages(id) ON DELETE CASCADE,
    part INTEGER NOT NULL,
    span TEXT,
    vector BLOB NOT NULL,
    PRIMARY KEY(head, model, passage, part)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rep_search_vectors_passage ON rep_search_vectors(passage);
CREATE TABLE IF NOT EXISTS rep_search_limited_media(
    head TEXT NOT NULL,
    model TEXT NOT NULL,
    passage INTEGER NOT NULL REFERENCES rep_search_passages(id) ON DELETE CASCADE,
    PRIMARY KEY(head, model, passage)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rep_search_limited_media_passage ON rep_search_limited_media(passage);
CREATE TRIGGER IF NOT EXISTS rep_search_object_arrival AFTER INSERT ON objects
BEGIN
    UPDATE rep_search_revision SET value=value+1;
    UPDATE rep_search_heads SET dirty=1 WHERE head IN (
        SELECT head FROM rep_search_waiting WHERE source=NEW.id
    );
END;
CREATE TRIGGER IF NOT EXISTS rep_search_object_withdrawal AFTER UPDATE OF data ON objects
WHEN OLD.data IS NOT NULL AND NEW.data IS NULL
BEGIN
    UPDATE rep_search_revision SET value=value+1;
    UPDATE rep_search_heads SET limited=1
        WHERE head IN (
            SELECT e.head FROM rep_search_entries e
            JOIN rep_search_sources s ON s.entry=e.id WHERE s.source=NEW.id
        );
    DELETE FROM rep_search_entries WHERE id IN (
        SELECT entry FROM rep_search_sources WHERE source=NEW.id
    );
END;
