use super::Bundle;
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
