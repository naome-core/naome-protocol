//! Content-bound evidence for classifying an unavailable ordinary occurrence.

use crate::{
    profile::{Document, Kind, PUBLIC, attest, verify_attestation},
    store::{Archive, Snapshot},
    wire::{Format, Id},
};
use anyhow::{Result, ensure};
use ed25519_dalek::SigningKey;
use std::collections::BTreeSet;

pub(crate) const ROLE: &str = "non-notice-occurrence";
pub(crate) const SCOPE: &str = "notice-coverage-1";
const CONTEXT: &str = "research-policy-1";

fn ordinary_occurrence(document: &Document) -> bool {
    document.kind == Kind::Occurrence && document.scope != "notice-horizon"
}

pub(super) fn trusted_keys(policy: &Document) -> Result<BTreeSet<String>> {
    policy
        .fields
        .get("keys")
        .and_then(|keys| keys.get(ROLE))
        .map(|keys| serde_json::from_value(keys.clone()).map_err(Into::into))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn verify_exclusion(
    attestation: &Document,
    target: Id,
    trusted: &BTreeSet<String>,
) -> Result<()> {
    ensure!(
        target.format().is_some(),
        "expected an occurrence record identifier"
    );
    verify_attestation(attestation, target, SCOPE, ROLE, CONTEXT, trusted)
}

impl Archive {
    /// Attest that an exact, readable record is an ordinary occurrence outside the notice horizon.
    pub fn certify_non_notice_occurrence(
        &self,
        snapshot: &Snapshot,
        target: Id,
        key: &SigningKey,
        format: Format,
    ) -> Result<Id> {
        let document = self.read(snapshot, target, "read")?;
        ensure!(
            ordinary_occurrence(&document),
            "notice exclusion requires an ordinary occurrence"
        );
        self.document(
            &attest(target, SCOPE, ROLE, CONTEXT, key)?,
            format,
            Id::blob(PUBLIC),
        )
    }

    /// Check a coverage attestation against an explicitly trusted key and the pinned inventory.
    /// The recipient may lack the occurrence bytes.
    pub fn verify_non_notice_occurrence(
        &self,
        snapshot: &Snapshot,
        target: Id,
        signature: Id,
        trusted: &BTreeSet<String>,
    ) -> Result<()> {
        self.with_read_view(|| {
            ensure!(
                snapshot.objects.contains(&target),
                "occurrence outside snapshot"
            );
            if self.readable(snapshot, target).is_ok() {
                let document = self.read(snapshot, target, "read")?;
                ensure!(
                    ordinary_occurrence(&document),
                    "notice exclusion contradicts the available record"
                );
            }
            verify_exclusion(&self.read(snapshot, signature, "read")?, target, trusted)
        })
    }
}
