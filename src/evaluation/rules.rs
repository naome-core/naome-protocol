use super::correction::WithdrawalRules;
use crate::{profile::Document, store::ManifestEncoding};
use anyhow::{Context, Result};

pub(crate) const V0_1_0_EVALUATOR_ID: &str = "rep-rust-0.1.0";
pub(crate) const FIXTURE_EVALUATOR_ID: &str = "rep-rust-rules-1";
const RECORD_BUDGET_EVALUATOR_ID: &str = "rep-rust-rules-5";

// The current writer selects one immutable replay policy by ID.
pub(crate) const EVALUATOR_ID: &str = "rep-rust-rules-7";

#[derive(PartialEq, Eq)]
pub(super) enum NoticeBudget {
    Objects,
    Records,
    CompactRecords,
}

pub(super) enum CorrectionRules {
    Disabled,
    InputVersions,
    AlwaysScoped,
    ScopedWhenConfigured,
}

pub(super) struct CorrectionActivation {
    pub(super) input_versions: bool,
    pub(super) scoped: bool,
}

impl CorrectionRules {
    pub(super) fn for_policy(&self, policy: &Document) -> CorrectionActivation {
        let has_input_keys = policy.fields.contains_key("input_correction_keys");
        CorrectionActivation {
            input_versions: match self {
                Self::Disabled => false,
                Self::InputVersions => true,
                Self::AlwaysScoped | Self::ScopedWhenConfigured => has_input_keys,
            },
            scoped: match self {
                Self::Disabled | Self::InputVersions => false,
                Self::AlwaysScoped => true,
                Self::ScopedWhenConfigured => {
                    has_input_keys || policy.fields.contains_key("foreign_calibration_keys")
                }
            },
        }
    }
}

pub(super) struct EvaluatorRules {
    ids: &'static [&'static str],
    pub(crate) manifests: ManifestEncoding,
    pub(super) notice_budget: NoticeBudget,
    pub(super) withdrawal: WithdrawalRules,
    pub(super) resolve_required_withdrawals: bool,
    pub(super) physical_identity: bool,
    pub(super) corrections: CorrectionRules,
}

const REPLAY_RULES: &[EvaluatorRules] = &[
    EvaluatorRules {
        ids: &[EVALUATOR_ID],
        manifests: ManifestEncoding::Canonical,
        notice_budget: NoticeBudget::CompactRecords,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::ScopedWhenConfigured,
    },
    EvaluatorRules {
        ids: &["rep-rust-rules-6"],
        manifests: ManifestEncoding::Canonical,
        notice_budget: NoticeBudget::Records,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::ScopedWhenConfigured,
    },
    EvaluatorRules {
        ids: &[RECORD_BUDGET_EVALUATOR_ID],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Records,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::ScopedWhenConfigured,
    },
    EvaluatorRules {
        ids: &["rep-rust-rules-4"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::AlwaysScoped,
    },
    EvaluatorRules {
        ids: &["rep-rust-rules-3"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::InputVersions,
    },
    EvaluatorRules {
        ids: &["rep-rust-rules-2"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: true,
        corrections: CorrectionRules::Disabled,
    },
    EvaluatorRules {
        ids: &[FIXTURE_EVALUATOR_ID],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: true,
        physical_identity: false,
        corrections: CorrectionRules::Disabled,
    },
    EvaluatorRules {
        ids: &["rep-rust-0.1.2", "rep-rust-0.1.2-dev.1"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::Scoped,
        resolve_required_withdrawals: false,
        physical_identity: false,
        corrections: CorrectionRules::Disabled,
    },
    EvaluatorRules {
        ids: &["rep-rust-0.1.2-dev"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::FirstScoped,
        resolve_required_withdrawals: false,
        physical_identity: false,
        corrections: CorrectionRules::Disabled,
    },
    EvaluatorRules {
        ids: &[V0_1_0_EVALUATOR_ID, "rep-rust-0.1.1"],
        manifests: ManifestEncoding::LegacyJson,
        notice_budget: NoticeBudget::Objects,
        withdrawal: WithdrawalRules::ScopeOnly,
        resolve_required_withdrawals: false,
        physical_identity: false,
        corrections: CorrectionRules::Disabled,
    },
];

impl EvaluatorRules {
    pub(super) fn for_id(evaluator: &str) -> Result<&'static Self> {
        REPLAY_RULES
            .iter()
            .find(|rules| rules.ids.contains(&evaluator))
            .context("unsupported receipt evaluator")
    }
}

pub(crate) fn manifest_encoding(evaluator: &str) -> Result<ManifestEncoding> {
    Ok(EvaluatorRules::for_id(evaluator)?.manifests)
}

#[cfg(feature = "cli")]
pub(crate) fn replay_evaluator_ids() -> impl Iterator<Item = &'static str> {
    REPLAY_RULES
        .iter()
        .flat_map(|rules| rules.ids.iter().copied())
}
