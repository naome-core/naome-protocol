CREATE TRIGGER rep_search_sample_new_vector AFTER INSERT ON rep_search_vectors BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE model=NEW.model AND NOT invalid
        AND head IN (SELECT v.head FROM rep_search_passages p JOIN rep_search_visible v ON v.entry=p.entry
                     WHERE p.id=NEW.passage);
END;
CREATE TRIGGER rep_search_sample_lost_vector AFTER DELETE ON rep_search_vectors BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE model=OLD.model AND NOT invalid
        AND head IN (SELECT v.head FROM rep_search_passages p JOIN rep_search_visible v ON v.entry=p.entry
                     WHERE p.id=OLD.passage);
END;
CREATE TRIGGER rep_search_sample_lost_passage BEFORE DELETE ON rep_search_passages BEGIN
    UPDATE rep_search_samples SET invalid=1 WHERE NOT invalid
        AND model IN (SELECT model FROM rep_search_vectors WHERE passage=OLD.id)
        AND head IN (SELECT head FROM rep_search_visible WHERE entry=OLD.entry);
END;
