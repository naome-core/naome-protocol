//! Content-bound classifications for the notice search in a partial archive.

use crate::{
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind, PUBLIC, attest, verify_attestation},
    store::{Archive, Snapshot},
    wire::{Format, Id},
};
use anyhow::{Result, ensure};
use ed25519_dalek::SigningKey;
use std::collections::BTreeSet;

pub(crate) const SCOPE: &str = "notice-coverage-1";
pub(super) const FRESHNESS_SCOPE: &str = "notice-horizon";
const CONTEXT: &str = "research-policy-1";

pub(super) enum SearchRole {
    Notice,
    Attestation,
    IdentityCheck,
    Freshness,
    Evidence,
}

pub(super) fn search_role(document: &Document) -> SearchRole {
    match document.kind {
        Kind::Notice => SearchRole::Notice,
        Kind::Attestation => SearchRole::Attestation,
        Kind::Assessment
            if document
                .fields
                .get("test")
                .and_then(serde_json::Value::as_str)
                == Some(CALIBRATION_EVENT_IDENTITY) =>
        {
            SearchRole::IdentityCheck
        }
        Kind::Occurrence if document.scope == FRESHNESS_SCOPE => SearchRole::Freshness,
        Kind::Profile
        | Kind::Question
        | Kind::Plan
        | Kind::Occurrence
        | Kind::Derivation
        | Kind::Claim
        | Kind::Argument
        | Kind::Assessment
        | Kind::Decision
        | Kind::Mapping
        | Kind::Label
        | Kind::Manifest
        | Kind::Model
        | Kind::Calibration
        | Kind::Guarantee
        | Kind::Projection
        | Kind::Policy
        | Kind::Authority
        | Kind::Closure
        | Kind::Selection
        | Kind::Descriptor => SearchRole::Evidence,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Exclusion {
    Occurrence,
    Record,
}

impl Exclusion {
    pub(crate) const fn role(self) -> &'static str {
        match self {
            Self::Occurrence => "non-notice-occurrence",
            Self::Record => "notice-search-exclusion",
        }
    }

    #[cfg(feature = "cli")]
    pub(crate) fn for_role(role: &str) -> Option<Self> {
        [Self::Record, Self::Occurrence]
            .into_iter()
            .find(|kind| kind.role() == role)
    }

    fn verify_record(self, document: &Document) -> Result<()> {
        match self {
            Self::Occurrence => ensure!(
                document.kind == Kind::Occurrence
                    && matches!(search_role(document), SearchRole::Evidence),
                "notice exclusion requires an ordinary occurrence"
            ),
            Self::Record => ensure!(
                matches!(search_role(document), SearchRole::Evidence),
                "record can contribute directly to the notice search"
            ),
        }
        Ok(())
    }

    fn trusted_keys(self, policy: &Document) -> Result<BTreeSet<String>> {
        policy
            .fields
            .get("keys")
            .and_then(|keys| keys.get(self.role()))
            .map(|keys| serde_json::from_value(keys.clone()).map_err(Into::into))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn verify_signature(
        self,
        attestation: &Document,
        target: Id,
        trusted: &BTreeSet<String>,
    ) -> Result<()> {
        ensure!(target.format().is_some(), "expected a record identifier");
        verify_attestation(attestation, target, SCOPE, self.role(), CONTEXT, trusted)
    }

    pub(crate) fn certify(
        self,
        archive: &Archive,
        snapshot: &Snapshot,
        target: Id,
        key: &SigningKey,
        format: Format,
    ) -> Result<Id> {
        self.verify_record(&archive.read(snapshot, target, "read")?)?;
        archive.document(
            &attest(target, SCOPE, self.role(), CONTEXT, key)?,
            format,
            Id::blob(PUBLIC),
        )
    }

    pub(crate) fn verify_at(
        self,
        archive: &Archive,
        snapshot: &Snapshot,
        target: Id,
        signature: Id,
        trusted: &BTreeSet<String>,
    ) -> Result<()> {
        archive.with_read_view(|| {
            ensure!(
                snapshot.objects.contains(&target),
                "record outside snapshot"
            );
            if archive.readable(snapshot, target).is_ok() {
                self.verify_record(&archive.read(snapshot, target, "read")?)
                    .map_err(|error| {
                        error.context("notice exclusion contradicts the available record")
                    })?;
            }
            self.verify_signature(&archive.read(snapshot, signature, "read")?, target, trusted)
        })
    }
}

pub(super) struct CoverageAuthority {
    pub(super) version: Exclusion,
    keys: Vec<(Exclusion, BTreeSet<String>)>,
}

impl CoverageAuthority {
    pub(super) fn from_policy(version: Exclusion, policy: &Document) -> Result<Option<Self>> {
        let mut keys = Vec::new();
        for kind in [Exclusion::Record, Exclusion::Occurrence] {
            if version == Exclusion::Occurrence && kind == Exclusion::Record {
                continue;
            }
            let trusted = kind.trusted_keys(policy)?;
            if !trusted.is_empty() {
                keys.push((kind, trusted));
            }
        }
        // Linked calibration identity discovery can depend on otherwise ordinary records.
        if policy.fields.contains_key("foreign_calibration_keys") {
            return Ok(None);
        }
        Ok((!keys.is_empty()).then_some(Self { version, keys }))
    }

    pub(super) fn admits(&self, attestation: &Document, target: Id) -> bool {
        self.keys
            .iter()
            .any(|(kind, trusted)| kind.verify_signature(attestation, target, trusted).is_ok())
    }
}

impl Archive {
    /// Certify that an exact recognized record is outside direct notice discovery.
    pub fn certify_notice_search_exclusion(
        &self,
        snapshot: &Snapshot,
        target: Id,
        key: &SigningKey,
        format: Format,
    ) -> Result<Id> {
        Exclusion::Record.certify(self, snapshot, target, key, format)
    }

    /// Verify the signed classification for a record whose bytes may be unavailable.
    pub fn verify_notice_search_exclusion(
        &self,
        snapshot: &Snapshot,
        target: Id,
        signature: Id,
        trusted: &BTreeSet<String>,
    ) -> Result<()> {
        Exclusion::Record.verify_at(self, snapshot, target, signature, trusted)
    }

    /// Attest that an exact, readable record is an ordinary occurrence outside the notice horizon.
    pub fn certify_non_notice_occurrence(
        &self,
        snapshot: &Snapshot,
        target: Id,
        key: &SigningKey,
        format: Format,
    ) -> Result<Id> {
        Exclusion::Occurrence.certify(self, snapshot, target, key, format)
    }

    /// Verify the occurrence-specific coverage attestation used by the previous evaluator.
    pub fn verify_non_notice_occurrence(
        &self,
        snapshot: &Snapshot,
        target: Id,
        signature: Id,
        trusted: &BTreeSet<String>,
    ) -> Result<()> {
        Exclusion::Occurrence.verify_at(self, snapshot, target, signature, trusted)
    }
}
