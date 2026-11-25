use super::{Bundle, decode_packet_hex};
use anyhow::Result;

#[test]
fn import_limit_counts_trailing_whitespace_and_rejects_truncated_json() -> Result<()> {
    let input = include_bytes!("../../../spec/examples/exchange.json");
    let limit = input.len() as u64;
    let parsed = Bundle::read_bounded(&input[..], limit)?;
    assert_eq!(parsed.head, parsed.snapshot.id()?);

    let mut oversized = input.to_vec();
    oversized.push(b' ');
    let err = Bundle::read_bounded(&oversized[..], limit).unwrap_err();
    assert_eq!(err.to_string(), "exchange byte limit");

    let complete = input.iter().rposition(|byte| *byte == b'}').unwrap();
    assert!(Bundle::read_bounded(&input[..complete], limit).is_err());
    Ok(())
}

#[test]
fn packet_hex_matches_the_standard_decoder_and_reuses_the_output() {
    let mut buffer = Vec::new();
    for encoded in [
        "0012abCD ef",
        "0123456789aAbBcCdDeEfF",
        "a",
        "41é",
        "aZ",
        "",
        "ff",
        "00",
    ] {
        let expected = hex::decode(encoded);
        let actual = decode_packet_hex(encoded, &mut buffer);
        assert_eq!(actual.map(|()| buffer.clone()), expected, "{encoded:?}");
    }
}

#[test]
fn bundle_membership_is_exact_and_independent_of_packet_order() -> Result<()> {
    let mut bundle = Bundle::read(&include_bytes!("../../../spec/examples/exchange.json")[..])?;
    bundle.packets.reverse();
    bundle.verify_inventory()?;
    let omitted = bundle.packets.pop().unwrap();
    bundle.omitted.insert(omitted.id);
    bundle.verify_inventory()?;
    bundle.omitted.remove(&omitted.id);
    bundle
        .omitted
        .insert(crate::wire::Id::blob(b"outside this inventory"));
    assert!(bundle.verify_inventory().is_err());
    bundle.omitted.clear();
    bundle.packets.push(omitted);
    bundle.packets[1].id = bundle.packets[0].id;
    assert!(bundle.verify_inventory().is_err());
    Ok(())
}

#[cfg(feature = "cli")]
#[test]
fn both_offline_writers_share_the_callers_custody_view() -> Result<()> {
    use crate::{profile::PUBLIC, store::Archive, wire::Id};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let reader = Archive::open(&path)?;
    reader.db.pragma_update(None, "journal_mode", "WAL")?;
    let writer = Archive::open(&path)?;
    let payload = reader.blob(b"research observation", Id::blob(PUBLIC))?;
    let snapshot = reader.snapshot("custody view", Default::default())?;

    reader.with_read_view(|| {
        reader.resolve(&snapshot, payload, "read")?;
        writer.withdraw(payload)?;
        let page = reader.export(&snapshot, "read")?;
        assert!(page.packets.iter().any(|packet| packet.id == payload));
        let mut stream = Vec::new();
        reader.export_stream(&snapshot, "read", &mut stream)?;
        let mut recipient = Archive::memory()?;
        recipient.import_stream(stream.as_slice())?;
        assert_eq!(
            recipient.resolve(&snapshot, payload, "read")?,
            b"research observation"
        );
        Ok(())
    })?;
    assert!(reader.export(&snapshot, "read")?.omitted.contains(&payload));
    Ok(())
}

#[test]
fn stream_budget_commits_the_complete_input_or_nothing() -> Result<()> {
    use crate::{profile::PUBLIC, store::Archive, wire::Id};
    let source = Archive::memory()?;
    let reading = source.blob(b"reading at noon", Id::blob(PUBLIC))?;
    let snapshot = source.snapshot("wire budget", Default::default())?;
    let mut stream = Vec::new();
    source.export_stream(&snapshot, "read", &mut stream)?;
    let mut receiver = Archive::memory()?;
    let maximum = stream.len() as u64;
    assert!(
        receiver
            .import_stream_with_limit(stream.as_slice(), maximum - 1)
            .is_err()
    );
    assert!(receiver.raw(reading)?.is_none());
    assert!(receiver.head(snapshot.id()?).is_err());
    assert_eq!(
        receiver.import_stream_with_limit(stream.as_slice(), maximum)?,
        snapshot.id()?
    );
    assert_eq!(
        receiver.resolve_head(snapshot.id()?, reading, "read")?,
        b"reading at noon"
    );
    Ok(())
}

#[test]
fn acknowledged_ranges_resume_after_reopen_and_retry_without_advancing_twice() -> Result<()> {
    use super::ExchangePage;
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::Archive,
        wire::Id,
    };
    let origin = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let mut readings = Vec::new();
    for ordinal in 0..75u32 {
        let access = if ordinal == 1 {
            Id::blob(READ_ONLY)
        } else {
            public
        };
        readings.push(origin.blob(&ordinal.to_be_bytes(), access)?);
    }
    let snapshot = origin.snapshot("paged read", Default::default())?;
    let head = snapshot.id()?;
    let withheld = readings[7];
    origin.withdraw(withheld)?;
    let fetch = |after| -> Result<ExchangePage> {
        let encoded = serde_json::to_vec(&origin.export_exchange_page(head, after, 6)?)?;
        ExchangePage::read(encoded.as_slice())
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receiver.sqlite");
    let first = fetch(None)?;
    let next = fetch(first.progress().continuation)?;
    let mut receiver = Archive::open(&path)?;
    assert!(
        receiver
            .import_pinned_exchange_page(Id::blob(b"other snapshot"), &first)
            .is_err()
    );
    assert!(receiver.import_exchange_page(&next).is_err());
    assert!(receiver.head(head).is_err());
    let initial = receiver.import_pinned_exchange_page(head, &first)?;
    assert_eq!(initial.continuation, first.progress().continuation);
    let status = &Archive::open_for_read(&path)?.exchange_import_status(head)?[0];
    assert!(status.verified);
    assert_eq!(status.cursor, initial.continuation);
    assert_eq!(status.examined, 6);
    assert_eq!(status.supplied, Some(initial.packets as u64));
    assert!(!status.complete);
    assert!(
        readings
            .iter()
            .all(|id| receiver.raw(*id).unwrap().is_none())
    );
    assert!(receiver.head(head).is_err());
    assert!(receiver.backlink_index(head)?.is_none());
    drop(receiver);
    let mut receiver = Archive::open(&path)?;
    let local = receiver.snapshot("local research", Default::default())?;
    assert!(readings.iter().all(|id| !local.objects.contains(id)));
    let mut progress = receiver.import_exchange_page(&next)?;
    assert_eq!(
        receiver.import_exchange_page(&first)?.continuation,
        progress.continuation
    );
    while !progress.complete {
        assert!(receiver.head(head).is_err());
        progress = receiver.import_exchange_page(&fetch(progress.continuation)?)?;
    }
    assert!(receiver.import_exchange_page(&next)?.complete);
    assert!(progress.continuation.is_none());
    let status = &receiver.exchange_import_status(head)?[0];
    assert!(status.complete);
    assert_eq!(status.examined, snapshot.objects.len() as u64);
    assert_eq!(status.supplied, Some(status.examined - 1));
    assert_eq!(status.omitted, Some(1));
    assert!(receiver.backlink_index(head)?.unwrap().complete);
    assert_eq!(receiver.head(head)?.objects, snapshot.objects);
    assert!(receiver.resolve_head(head, readings[1], "train").is_err());
    let mut relay = Archive::memory()?;
    let forwarded = receiver.export_exchange_page(head, None, 6)?;
    assert_eq!(
        relay
            .import_pinned_exchange_page(head, &forwarded)?
            .continuation,
        first.progress().continuation
    );
    for (ordinal, id) in readings.into_iter().enumerate() {
        if id == withheld {
            assert!(receiver.resolve_head(head, id, "read").is_err());
        } else {
            assert_eq!(
                receiver.resolve_head(head, id, "read")?,
                (ordinal as u32).to_be_bytes()
            );
        }
    }
    Ok(())
}

#[test]
fn verified_exchange_with_an_omitted_record_keeps_backlink_coverage_open() -> Result<()> {
    use crate::{
        profile::{Document, Kind, PUBLIC, link},
        store::Archive,
        wire::{Format, Id},
    };
    use serde_json::json;

    let sender = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let question = sender.document(
        &Document::new(Kind::Question, "example", "scope", json!({}), vec![])?,
        Format::A,
        public,
    )?;
    let plan = sender.document(
        &Document::new(
            Kind::Plan,
            "example",
            "scope",
            json!({}),
            vec![link("question", question)],
        )?,
        Format::A,
        public,
    )?;
    let head = sender.snapshot_id("notebook", Default::default())?;
    // Model a sender whose inventory is pinned but whose record bytes are unavailable.
    sender.db.execute(
        "UPDATE objects SET data=NULL WHERE id=?",
        [plan.to_string()],
    )?;

    let mut receiver = Archive::memory()?;
    let page = sender.export_exchange_page(head, None, 64)?;
    assert!(receiver.import_pinned_exchange_page(head, &page)?.complete);
    assert!(!receiver.backlink_index(head)?.unwrap().complete);
    assert!(
        receiver
            .question_at(Some(head), question, 2)?
            .limited_coverage
    );
    assert!(receiver.read_head(head, plan, "read").is_err());
    Ok(())
}

#[test]
fn historical_ranges_require_trusted_compatibility_mode() -> Result<()> {
    use crate::{profile::PUBLIC, store::Archive, wire::Id};
    let source = Archive::memory()?;
    let reading = source.blob(b"historical reading", Id::blob(PUBLIC))?;
    let snapshot =
        source.selected_snapshot("historic sender", Default::default(), [reading].into())?;
    let head = snapshot.id()?;
    let mut receiver = Archive::memory()?;
    let first = source.export_legacy_exchange_page(head, None, 1)?;
    assert!(receiver.import_pinned_exchange_page(head, &first).is_err());
    let progress = receiver.import_legacy_exchange_page(head, &first)?;
    assert!(!progress.complete);
    let next = source.export_legacy_exchange_page(head, progress.continuation, 1)?;
    assert!(receiver.import_legacy_exchange_page(head, &next)?.complete);
    let verified = source.export_exchange_page(head, None, 1)?;
    assert!(
        !receiver
            .import_pinned_exchange_page(head, &verified)?
            .complete
    );
    let sessions = receiver.exchange_import_status(head)?;
    assert_eq!(sessions.len(), 2);
    assert!(sessions[0].verified && !sessions[0].complete);
    assert!(!sessions[1].verified && sessions[1].complete);
    assert_eq!(sessions[1].supplied, Some(snapshot.objects.len() as u64));
    assert_eq!(
        receiver.resolve_head(head, reading, "read")?,
        b"historical reading"
    );
    Ok(())
}

#[test]
fn custody_witness_distinguishes_read_only_and_public_evidence() -> Result<()> {
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::{Archive, custody::tree::Tree},
        wire::Id,
    };
    let archive = Archive::memory()?;
    let read_only = Id::blob(READ_ONLY);
    let reading = archive.blob(b"permitted for reading", read_only)?;
    let snapshot =
        archive.selected_snapshot("access check", Default::default(), [reading].into())?;
    let descriptor = archive.custody_head(snapshot.id()?)?;
    let mut tree = Tree::new(&archive.db);
    super::page::verify_access(&descriptor, reading, read_only, |id| tree.read(id))?;
    assert!(
        super::page::verify_access(&descriptor, reading, Id::blob(PUBLIC), |id| tree.read(id))
            .is_err()
    );
    Ok(())
}

#[test]
fn disclosed_historical_inventory_is_reachable_in_each_read_exchange() -> Result<()> {
    use crate::{profile::PUBLIC, store::Archive, wire::Id};
    let origin = Archive::memory()?;
    for sample in 0u32..40 {
        origin.blob(&sample.to_le_bytes(), Id::blob(PUBLIC))?;
    }
    let historical = origin.snapshot("instrument history", Default::default())?;
    let horizon = origin.deposit_manifest(&historical)?;
    let disclosure =
        origin.selected_snapshot("receipt inputs", Default::default(), [horizon].into())?;
    let head = disclosure.id()?;
    for mode in ["bundle", "stream", "pages"] {
        let mut receiver = Archive::memory()?;
        match mode {
            "bundle" => {
                receiver.import(&origin.export(&disclosure, "read")?)?;
            }
            "stream" => {
                let mut wire = Vec::new();
                origin.export_stream_head(head, "read", &mut wire)?;
                receiver.import_stream(wire.as_slice())?;
            }
            _ => {
                let mut cursor = None;
                loop {
                    let page = origin.export_exchange_page(head, cursor, 1)?;
                    let ack = receiver.import_exchange_page(&page)?;
                    if ack.complete {
                        break;
                    }
                    cursor = ack.continuation;
                }
            }
        }
        assert_eq!(
            receiver.resolve_head(head, horizon, "read")?,
            historical.bytes()?
        );
        let (members, _) = historical.manifest()?.read(&receiver.db)?;
        assert_eq!(members, historical.objects);
    }
    Ok(())
}

#[test]
fn a_witness_needs_a_verified_inventory_before_commit() -> Result<()> {
    use super::{Archive, Witnesses, inventory::Page};
    use crate::wire::Id;
    let mut receiver = Archive::memory()?;
    let count = |archive: &Archive| {
        archive
            .db
            .query_row("SELECT count(*) FROM custody_pages", [], |r| {
                r.get::<_, i64>(0)
            })
    };
    let before = count(&receiver)?;
    {
        let tx = receiver.db.transaction()?;
        let pending = Witnesses::begin(&tx)?;
        let page = Page::Leaf(Vec::new());
        pending.install(Id::blob(&serde_json::to_vec(&page)?), &page)?;
        assert!(pending.finish().is_err());
    }
    assert_eq!(count(&receiver)?, before);
    Ok(())
}

#[test]
fn pending_imports_share_bytes_and_respect_intervening_admission_and_withdrawal() -> Result<()> {
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::Archive,
        wire::Id,
    };
    let source = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let mut payloads = std::collections::BTreeMap::new();
    for n in 0..10u8 {
        let bytes = vec![n; 24];
        payloads.insert(source.blob(&bytes, public)?, bytes);
    }
    let (&payload, bytes) = payloads.first_key_value().unwrap();
    let first = source.snapshot("first transfer", Default::default())?;
    let second = source.snapshot("second transfer", Default::default())?;
    let heads = [first.id()?, second.id()?];
    let limit = first.objects.len() - 1;
    let mut receiver = Archive::memory()?;
    let mut final_pages = Vec::new();
    for head in heads {
        let start = source.export_exchange_page(head, None, limit)?;
        let progress = receiver.import_pinned_exchange_page(head, &start)?;
        assert!(!progress.complete);
        assert!(receiver.raw(payload)?.is_none());
        final_pages.push(source.export_exchange_page(head, progress.continuation, limit)?);
    }
    let copies: i64 = receiver.db.query_row(
        "SELECT (SELECT count(*) FROM archive_extent_blocks WHERE id=?1)
              + (SELECT count(*) FROM archive_extents WHERE id=?1 AND inline_data IS NOT NULL)",
        [payload.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(copies, 1);

    assert_eq!(receiver.blob(bytes, public)?, payload);
    assert_eq!(&receiver.resolve_local(payload)?, bytes);
    receiver.withdraw(payload)?;
    for (head, page) in heads.into_iter().zip(&final_pages) {
        assert!(receiver.import_pinned_exchange_page(head, page)?.complete);
        assert!(receiver.resolve_head(head, payload, "read").is_err());
        let status = &receiver.exchange_import_status(head)?[0];
        assert_eq!(status.supplied, Some(status.examined));
    }

    for verified in [true, false] {
        let mut conflicted = Archive::memory()?;
        let head = heads[0];
        let export = |after| {
            if verified {
                source.export_exchange_page(head, after, limit)
            } else {
                source.export_legacy_exchange_page(head, after, limit)
            }
        };
        let import = |archive: &mut Archive, page: &_| {
            if verified {
                archive.import_pinned_exchange_page(head, page)
            } else {
                archive.import_legacy_exchange_page(head, page)
            }
        };
        let initial = export(None)?;
        let pending = import(&mut conflicted, &initial)?;
        conflicted.blob(bytes, Id::blob(READ_ONLY))?;
        let last = export(pending.continuation)?;
        assert!(import(&mut conflicted, &last)?.complete);
        assert!(import(&mut conflicted, &initial)?.complete);
        let status = &conflicted.exchange_import_status(head)?[0];
        assert_eq!(status.omitted, Some(1));
        assert_eq!(status.supplied, Some(status.examined - 1));
        assert!(conflicted.resolve_head(head, payload, "read").is_err());
        assert_eq!(conflicted.resolve_local(payload)?, *bytes);
        for &other in payloads.keys().filter(|&&id| id != payload) {
            assert_eq!(
                conflicted.resolve_head(head, other, "read")?,
                payloads[&other]
            );
        }
    }
    Ok(())
}

#[test]
fn a_pending_import_resumes_from_the_previous_blob_staging_layout() -> Result<()> {
    use crate::{
        profile::{Document, Kind, PUBLIC},
        store::Archive,
        wire::{Format, Id},
    };
    let source = Archive::memory()?;
    for n in 0..12 {
        source.document(
            &Document::new(
                Kind::Question,
                "historical staging",
                "migration",
                serde_json::json!({"statement":format!("Question {n}")}),
                vec![],
            )?,
            Format::A,
            Id::blob(PUBLIC),
        )?;
    }
    let snapshot = source.snapshot("archive migration", Default::default())?;
    let head = snapshot.id()?;
    let completed = Id::blob(b"old completed head");
    let page = source.export_exchange_page(head, None, 8)?;
    let json = serde_json::to_value(&page)?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("previous.sqlite");
    let mut receiver = Archive::open(&path)?;
    let initial = receiver.import_pinned_exchange_page(head, &page)?;
    assert!(!initial.complete);

    // Recreate the exact prior checkpoint layout from the accepted page.
    receiver.db.execute_batch(
        "ALTER TABLE archive_exchange_sessions DROP COLUMN supplied;
         CREATE TABLE archive_exchange_packets(
            head TEXT NOT NULL, verified INTEGER NOT NULL, id TEXT NOT NULL, access TEXT NOT NULL,
            data BLOB NOT NULL, PRIMARY KEY(head,verified,id)
         ) WITHOUT ROWID;
         DROP TABLE archive_exchange_staged;
         DELETE FROM archive_extent_blocks;
         DELETE FROM archive_extents;
         ALTER TABLE archive_extents DROP COLUMN inline_data;",
    )?;
    receiver.db.execute(
        "INSERT INTO archive_exchange_sessions(head,verified,cursor,seen,complete)
         VALUES(?,1,NULL,1,1)",
        [completed.to_string()],
    )?;
    for packet in json["packets"].as_array().unwrap() {
        receiver.db.execute(
            "INSERT INTO archive_exchange_packets VALUES(?,1,?,?,?)",
            rusqlite::params![
                head.to_string(),
                packet["id"].as_str().unwrap(),
                packet["access"].as_str().unwrap(),
                hex::decode(packet["hex"].as_str().unwrap())?,
            ],
        )?;
    }
    drop(receiver);

    let reader = Archive::open_for_read(&path)?;
    let before = reader.exchange_import_status(head)?;
    assert_eq!(before[0].supplied, Some(initial.packets as u64));
    assert_eq!(reader.exchange_import_status(completed)?[0].supplied, None);
    drop(reader);

    let mut receiver = Archive::open(&path)?;
    assert_eq!(receiver.exchange_import_status(head)?, before);
    assert_eq!(receiver.exchange_import_status(completed)?[0].omitted, None);
    assert!(receiver.head(head).is_err());
    let last = source.export_exchange_page(head, initial.continuation, 64)?;
    assert!(receiver.import_pinned_exchange_page(head, &last)?.complete);
    assert_eq!(receiver.head(head)?.objects, snapshot.objects);
    for id in snapshot
        .objects
        .iter()
        .copied()
        .filter(|id| id.format().is_some())
    {
        assert_eq!(
            receiver.resolve_head(head, id, "read")?,
            source.resolve_head(head, id, "read")?
        );
    }
    Ok(())
}

#[test]
fn a_read_handle_keeps_resolving_payloads_across_the_extent_upgrade() -> Result<()> {
    use crate::{
        profile::PUBLIC,
        store::{Archive, custody::BLOCK},
        wire::Id,
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receiver.sqlite");
    let public = Id::blob(PUBLIC);
    let receiver = Archive::open(&path)?;
    let large = vec![0x75; BLOCK + 1];
    let blocked = receiver.blob(&large, public)?;
    receiver
        .db
        .execute_batch("ALTER TABLE archive_extents DROP COLUMN inline_data;")?;
    drop(receiver);

    let reader = Archive::open_for_read(&path)?;
    assert_eq!(reader.resolve_local(blocked)?, large);

    let source = Archive::memory()?;
    let tiny_bytes = b"short observation";
    let tiny = source.blob(tiny_bytes, public)?;
    let empty = source.blob(b"", public)?;
    let head = source.snapshot_id("inline transfer", Default::default())?;
    let mut receiver = Archive::open(&path)?;
    let page = source.export_exchange_page(head, None, 64)?;
    assert!(receiver.import_pinned_exchange_page(head, &page)?.complete);

    assert_eq!(reader.resolve_head(head, tiny, "read")?, tiny_bytes);
    assert_eq!(reader.resolve_head(head, empty, "read")?, b"");
    assert_eq!(reader.resolve_local(blocked)?, large);
    receiver.withdraw(tiny)?;
    assert!(reader.resolve_head(head, tiny, "read").is_err());
    Ok(())
}

#[test]
fn omitted_blobs_cannot_inherit_a_conflicting_local_acl() -> Result<()> {
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::Archive,
        wire::Id,
    };
    let source = Archive::memory()?;
    let bytes = b"read-only from the publisher, public only in the local archive";
    let payload = source.blob(bytes, Id::blob(READ_ONLY))?;
    let historical = source.snapshot("publisher historical snapshot", Default::default())?;
    let historical_id = source.deposit_manifest(&historical)?;
    let cut = source.snapshot("publisher", [historical_id].into())?;
    let head = cut.id()?;
    source.withdraw(payload)?;

    let mut receiver = Archive::memory()?;
    assert_eq!(receiver.blob(bytes, Id::blob(PUBLIC))?, payload);
    let local = receiver.snapshot("local", Default::default())?;
    let page = source.export_exchange_page(head, None, 64)?;
    assert!(receiver.import_pinned_exchange_page(head, &page)?.complete);
    let scoped = receiver.head(head)?;
    assert!(receiver.resolve_head(head, payload, "train").is_err());
    assert!(receiver.resolve(&scoped, payload, "train").is_err());
    assert!(receiver.readable(&scoped, payload).is_err());
    let linked = receiver.linked_snapshot(
        &scoped,
        historical_id,
        crate::store::ManifestEncoding::Canonical,
    )?;
    assert!(receiver.resolve(&linked, payload, "train").is_err());
    assert_eq!(receiver.resolve(&local, payload, "train")?, bytes);
    Ok(())
}

#[test]
fn conflicting_packet_does_not_advance_either_exchange_mode() -> Result<()> {
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::Archive,
        wire::Id,
    };
    let source = Archive::memory()?;
    let bytes = b"access conflict before the first page";
    let key = source.blob(bytes, Id::blob(READ_ONLY))?;
    let cut = source.snapshot("source", Default::default())?;
    let head = cut.id()?;
    for verified in [true, false] {
        let mut receiver = Archive::memory()?;
        receiver.blob(bytes, Id::blob(PUBLIC))?;
        let export = |source: &Archive| {
            if verified {
                source.export_exchange_page(head, None, 64)
            } else {
                source.export_legacy_exchange_page(head, None, 64)
            }
        };
        let import = |receiver: &mut Archive, page: &_| {
            if verified {
                receiver.import_pinned_exchange_page(head, page)
            } else {
                receiver.import_legacy_exchange_page(head, page)
            }
        };
        let page = export(&source)?;
        assert!(import(&mut receiver, &page).is_err());
        assert!(receiver.exchange_import_status(head)?.is_empty());
        assert!(receiver.head(head).is_err());
    }
    source.withdraw(key)?;
    let mut receiver = Archive::memory()?;
    receiver.blob(bytes, Id::blob(PUBLIC))?;
    let page = source.export_exchange_page(head, None, 64)?;
    assert!(receiver.import_pinned_exchange_page(head, &page)?.complete);
    assert!(receiver.resolve_head(head, key, "read").is_err());
    Ok(())
}
