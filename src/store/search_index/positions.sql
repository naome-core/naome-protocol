CREATE TABLE rep_search_samples(
    id INTEGER PRIMARY KEY, head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    model TEXT NOT NULL, object TEXT NOT NULL DEFAULT '', section INTEGER NOT NULL DEFAULT -1,
    location TEXT NOT NULL DEFAULT '', part INTEGER NOT NULL DEFAULT -1,
    count INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0,
    invalid INTEGER NOT NULL DEFAULT 0, UNIQUE(head,model)
);
CREATE TABLE rep_search_positions(
    sample INTEGER NOT NULL REFERENCES rep_search_samples(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL, passage INTEGER NOT NULL, part INTEGER NOT NULL,
    PRIMARY KEY(sample,ordinal)
) WITHOUT ROWID;
CREATE TRIGGER rep_search_sample_new_entry AFTER INSERT ON rep_search_set_entries BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE head IN(SELECT head FROM rep_search_head_sets WHERE set_id=NEW.set_id) AND NOT invalid;
END;
CREATE TRIGGER rep_search_sample_lost_entry AFTER DELETE ON rep_search_set_entries BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE head IN(SELECT head FROM rep_search_head_sets WHERE set_id=OLD.set_id) AND NOT invalid;
END;
CREATE TRIGGER rep_search_sample_new_set AFTER INSERT ON rep_search_head_sets BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE head=NEW.head AND NOT invalid;
END;
CREATE TRIGGER rep_search_sample_lost_set AFTER DELETE ON rep_search_head_sets BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE head=OLD.head AND NOT invalid;
END;
