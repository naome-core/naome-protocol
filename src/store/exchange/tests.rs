use super::{Bundle, decode_packet_hex};
use anyhow::Result;

#[test]
fn import_limit_counts_trailing_whitespace_and_rejects_truncated_json() -> Result<()> {
    let input = include_bytes!("../../../spec/examples/exchange-v1.json");
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
    let mut bundle = Bundle::read(&include_bytes!("../../../spec/examples/exchange-v1.json")[..])?;
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
