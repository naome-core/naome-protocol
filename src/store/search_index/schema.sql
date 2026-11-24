CREATE TABLE rep_search_schema(version INTEGER PRIMARY KEY CHECK(version=7));
INSERT INTO rep_search_schema VALUES(7);
CREATE TABLE rep_search_revision(value INTEGER NOT NULL);
INSERT INTO rep_search_revision VALUES(0);
CREATE TABLE rep_search_heads(
    head TEXT PRIMARY KEY,
    cursor TEXT,
    finished INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE rep_search_waiting(
    source TEXT NOT NULL,
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    object TEXT NOT NULL,
    ready INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(source,head,object)
) WITHOUT ROWID;
CREATE INDEX rep_search_waiting_head ON rep_search_waiting(head,ready,object);
CREATE TABLE rep_search_gaps(
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    object TEXT NOT NULL,
    PRIMARY KEY(head,object)
) WITHOUT ROWID;
CREATE TABLE rep_search_entries(
    id INTEGER PRIMARY KEY,
    object TEXT NOT NULL,
    section INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    subject TEXT NOT NULL,
    scope TEXT NOT NULL,
    coverage INTEGER,
    UNIQUE(object,section)
);
CREATE TABLE rep_search_text(
    entry INTEGER PRIMARY KEY REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    cursor INTEGER NOT NULL DEFAULT 0,
    length INTEGER NOT NULL CHECK(length>0),
    CHECK(cursor>=0 AND cursor<=length)
);
CREATE TABLE rep_search_sources(
    source TEXT NOT NULL,
    entry INTEGER NOT NULL REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    PRIMARY KEY(source,entry)
) WITHOUT ROWID;
CREATE INDEX rep_search_sources_entry ON rep_search_sources(entry);
CREATE TABLE rep_search_passages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry INTEGER NOT NULL REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    location TEXT NOT NULL,
    text TEXT NOT NULL,
    normalized TEXT NOT NULL,
    priority INTEGER NOT NULL,
    semantic INTEGER NOT NULL
);
CREATE INDEX rep_search_passages_entry ON rep_search_passages(entry,location);
CREATE TABLE rep_search_terms(
    term TEXT NOT NULL,
    passage INTEGER NOT NULL REFERENCES rep_search_passages(id) ON DELETE CASCADE,
    PRIMARY KEY(term,passage)
) WITHOUT ROWID;
CREATE INDEX rep_search_terms_passage ON rep_search_terms(passage);
CREATE TABLE rep_search_vector_state(
    passage INTEGER NOT NULL REFERENCES rep_search_passages(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    limited INTEGER NOT NULL,
    retry_after INTEGER,
    PRIMARY KEY(passage,model)
) WITHOUT ROWID;
CREATE TABLE rep_search_vectors(
    passage INTEGER NOT NULL,
    model TEXT NOT NULL,
    part INTEGER NOT NULL,
    span TEXT,
    vector BLOB NOT NULL,
    PRIMARY KEY(passage,model,part),
    FOREIGN KEY(passage,model) REFERENCES rep_search_vector_state(passage,model) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX rep_search_vector_keys ON rep_search_vectors(passage,model,part);
CREATE TRIGGER rep_search_object_arrival AFTER INSERT ON objects
BEGIN
    UPDATE rep_search_revision SET value=value+1;
    UPDATE rep_search_waiting SET ready=1 WHERE source=NEW.id;
END;
