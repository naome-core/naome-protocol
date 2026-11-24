CREATE TABLE rep_search_sets(id INTEGER PRIMARY KEY, writer TEXT UNIQUE);
CREATE TABLE rep_search_set_entries(
    set_id INTEGER NOT NULL REFERENCES rep_search_sets(id) ON DELETE CASCADE,
    entry INTEGER NOT NULL REFERENCES rep_search_entries(id) ON DELETE CASCADE,
    object TEXT NOT NULL, section INTEGER NOT NULL, PRIMARY KEY(set_id,entry)
) WITHOUT ROWID;
CREATE INDEX rep_search_set_entries_order ON rep_search_set_entries(set_id,object,section);
CREATE INDEX rep_search_set_entries_entry ON rep_search_set_entries(entry);
CREATE TABLE rep_search_head_sets(
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    set_id INTEGER NOT NULL REFERENCES rep_search_sets(id), PRIMARY KEY(head,set_id)
) WITHOUT ROWID;
CREATE INDEX rep_search_head_sets_by_set ON rep_search_head_sets(set_id);
CREATE VIEW rep_search_visible AS
    SELECT h.head,s.entry,s.object,s.section FROM rep_search_head_sets h
    JOIN rep_search_set_entries s ON s.set_id=h.set_id;
CREATE TRIGGER rep_search_release_set AFTER DELETE ON rep_search_head_sets
WHEN NOT EXISTS(SELECT 1 FROM rep_search_head_sets WHERE set_id=OLD.set_id)
BEGIN DELETE FROM rep_search_sets WHERE id=OLD.set_id; END;
DROP TRIGGER IF EXISTS rep_search_object_withdrawal;
CREATE TRIGGER rep_search_object_withdrawal AFTER UPDATE OF data ON objects
WHEN OLD.data IS NOT NULL AND NEW.data IS NULL
BEGIN
    UPDATE rep_search_revision SET value=value+1;
    INSERT OR IGNORE INTO rep_search_gaps(head,object)
        SELECT v.head,e.object FROM rep_search_entries e
        JOIN rep_search_visible v ON v.entry=e.id
        JOIN rep_search_sources s ON s.entry=e.id WHERE s.source=NEW.id;
    UPDATE rep_search_entries SET coverage=NULL WHERE section=0 AND object IN (
        SELECT e.object FROM rep_search_entries e JOIN rep_search_sources s ON s.entry=e.id
        WHERE s.source=NEW.id
    );
    DELETE FROM rep_search_entries WHERE id IN (
        SELECT entry FROM rep_search_sources WHERE source=NEW.id
    );
END;
