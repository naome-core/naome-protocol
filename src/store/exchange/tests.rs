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
    use crate::{profile::PUBLIC, store::Archive, wire::Id};
    let origin = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let mut readings = Vec::new();
    for ordinal in 0..75u32 {
        readings.push(origin.blob(&ordinal.to_be_bytes(), public)?);
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
    assert!(receiver.import_exchange_page(&next).is_err());
    assert!(receiver.head(head).is_err());
    let initial = receiver.import_exchange_page(&first)?;
    assert_eq!(initial.continuation, first.progress().continuation);
    assert!(receiver.head(head).is_err());
    drop(receiver);
    let mut receiver = Archive::open(&path)?;
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
    assert_eq!(receiver.head(head)?.objects, snapshot.objects);
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
