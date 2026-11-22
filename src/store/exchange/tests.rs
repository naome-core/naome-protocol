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
