use crate::{
    migration::Equivalence,
    profile::{Document, Kind},
    wire::Id,
};
use anyhow::{Context, Result, anyhow, ensure};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WithdrawalRules {
    ScopeOnly,
    FirstScoped,
    Scoped,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct MeasurementUse<'a> {
    mode: Option<&'a str>,
    event: Option<i64>,
}

impl<'a> MeasurementUse<'a> {
    pub(crate) fn from_reading(reading: &'a Document, rules: WithdrawalRules) -> Self {
        let mode = reading.fields.get("mode").and_then(|v| v.as_str());
        Self {
            // The first scoped evaluator treated an empty direct reading as missing.
            mode: mode.filter(|mode| rules != WithdrawalRules::FirstScoped || !mode.is_empty()),
            event: reading.fields.get("event").and_then(|v| v.as_i64()),
        }
    }

    pub(crate) fn for_argument(
        argument: &Document,
        reading: impl FnOnce(Id) -> Option<&'a Document>,
        rules: WithdrawalRules,
    ) -> Option<Self> {
        match argument
            .fields
            .get("method")
            .and_then(serde_json::Value::as_str)
        {
            Some("recorded-difference" | "simultaneous-bounds") => Some(
                argument
                    .roles("reading")
                    .next()
                    .and_then(reading)
                    .map(|doc| Self::from_reading(doc, rules))
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    pub(super) fn measured(mode: &'a str, event: i64) -> Self {
        Self {
            mode: Some(mode),
            event: Some(event),
        }
    }

    pub(crate) fn complete(self) -> bool {
        self.mode.is_some() && self.event.is_some()
    }

    pub(crate) fn matches_mode(self, mode: &str) -> bool {
        self.mode == Some(mode)
    }

    fn outside_mode(self, required: &str) -> bool {
        self.mode.is_some_and(|mode| mode != required)
    }

    fn outside_interval(self, bounds: [i64; 2]) -> bool {
        self.event
            .is_some_and(|event| !(bounds[0]..=bounds[1]).contains(&event))
    }
}

pub(crate) enum WithdrawalApplicability {
    Applies,
    Excluded,
    Unknown(anyhow::Error),
}

/// A notice's target and scope rules, independent of recipient admission.
#[derive(Clone, Copy)]
pub(crate) struct Notice<'a>(pub(crate) &'a Document);

impl<'a> Notice<'a> {
    pub(crate) fn in_scope(self, scope: &str) -> bool {
        self.0.scope == scope
    }

    fn targets(self) -> impl Iterator<Item = Id> + 'a {
        self.0.roles("target")
    }

    pub(crate) fn target_endpoints(self, aliases: &Equivalence) -> BTreeSet<Id> {
        self.targets().flat_map(|id| aliases.members(id)).collect()
    }

    pub(crate) fn matching_targets(
        self,
        aliases: &'a Equivalence,
        premises: &'a BTreeSet<Id>,
    ) -> impl Iterator<Item = Id> + 'a {
        self.targets()
            .filter(move |&id| touches(id, aliases, premises))
    }

    // Delegated receipts also depend on links in later notices outside the
    // direct target role, such as a calibration link.
    pub(super) fn refers_to(self, aliases: &Equivalence, premises: &BTreeSet<Id>) -> bool {
        self.0
            .inputs
            .iter()
            .any(|link| touches(link.target, aliases, premises))
    }

    pub(crate) fn withdrawal(
        self,
        scope: &str,
        measurement: MeasurementUse<'_>,
        rules: WithdrawalRules,
    ) -> WithdrawalApplicability {
        if !self.in_scope(scope) {
            return WithdrawalApplicability::Excluded;
        }
        let mode = self
            .0
            .fields
            .get("mode")
            .map(|value| {
                value
                    .as_str()
                    .filter(|mode| !mode.is_empty())
                    .context("invalid withdrawal mode")
            })
            .transpose();
        let interval = self
            .0
            .fields
            .get("interval")
            .map(|_| {
                let [first, last] = self.0.field::<[i64; 2]>("interval")?;
                ensure!(first <= last, "reversed withdrawal interval");
                Ok([first, last])
            })
            .transpose();

        use WithdrawalApplicability::{Applies, Excluded, Unknown};
        match (mode, interval) {
            (Err(error), _) | (_, Err(error)) if rules == WithdrawalRules::FirstScoped => {
                Unknown(error)
            }
            (Ok(Some(required)), _) if measurement.outside_mode(required) => Excluded,
            (_, Ok(Some(bounds))) if measurement.outside_interval(bounds) => Excluded,
            (Err(error), _) | (_, Err(error)) => Unknown(error),
            (Ok(Some(_)), _) if measurement.mode.is_none() => {
                Unknown(anyhow!("withdrawal needs a measurement mode"))
            }
            (_, Ok(Some(_))) if measurement.event.is_none() => {
                Unknown(anyhow!("withdrawal needs a measurement event"))
            }
            _ => Applies,
        }
    }

    pub(crate) fn excludes_current_withdrawal(
        self,
        scope: &str,
        measurement: MeasurementUse<'_>,
    ) -> bool {
        matches!(
            self.withdrawal(scope, measurement, WithdrawalRules::Scoped),
            WithdrawalApplicability::Excluded
        )
    }

    pub(crate) fn calibration_applies(
        self,
        scope: &str,
        measurement: MeasurementUse<'_>,
    ) -> Result<bool> {
        // Receipt replay checks the interval before scope, and only reads the
        // mode when this calibration notice has the requested scope.
        let interval = self.0.field("interval")?;
        Ok(self.in_scope(scope)
            && !measurement.outside_mode(&self.0.field::<String>("mode")?)
            && !measurement.outside_interval(interval))
    }

    pub(crate) fn calibration_evidence<'b>(
        self,
        read: impl FnOnce(Id) -> Result<&'b Document>,
    ) -> Result<Id> {
        let id = self.0.one("evidence")?;
        let report = read(id)?;
        require_micrometres(self.0)?;
        require_micrometres(report)?;
        ensure!(
            report.kind == Kind::Occurrence
                && report.scope == self.0.scope
                && report.field::<i64>("bias")? == self.0.field::<i64>("bias")?,
            "notice evidence disagrees"
        );
        Ok(id)
    }

    pub(crate) fn calibration_limits(self) -> Result<CalibrationLimits<'a>> {
        Ok(CalibrationLimits {
            notice: self,
            mode: self.0.field("mode")?,
            interval: self.0.field("interval")?,
        })
    }
}

/// A source notice whose target may be carried over a physical identity link.
pub(crate) struct LinkedCalibrationNotice<'a> {
    notice: Notice<'a>,
    pub source: Id,
    pub withdrawal: bool,
}

impl<'a> LinkedCalibrationNotice<'a> {
    pub(crate) fn parse(document: &'a Document) -> Option<Self> {
        let notice = Notice(document);
        if document.roles("calibration").next().is_some() {
            return Some(Self {
                notice,
                source: document.one("calibration").ok()?,
                withdrawal: false,
            });
        }
        if document
            .fields
            .get("action")
            .and_then(serde_json::Value::as_str)
            != Some("withdraw")
        {
            return None;
        }
        Some(Self {
            notice,
            source: document.one("target").ok()?,
            withdrawal: true,
        })
    }

    pub(crate) fn signature_role(&self) -> &'static str {
        if self.withdrawal {
            "notice"
        } else {
            "calibration"
        }
    }

    pub(crate) fn applicability(&self, measurement: MeasurementUse<'_>) -> WithdrawalApplicability {
        use WithdrawalApplicability::{Applies, Excluded, Unknown};
        let scope = &self.notice.0.scope;
        if self.withdrawal {
            self.notice
                .withdrawal(scope, measurement, WithdrawalRules::Scoped)
        } else {
            match self.notice.calibration_limits() {
                Ok(limits) if limits.mode.is_empty() || limits.interval[0] > limits.interval[1] => {
                    Unknown(anyhow!("invalid linked calibration correction scope"))
                }
                Ok(limits)
                    if measurement.outside_mode(&limits.mode)
                        || measurement.outside_interval(limits.interval) =>
                {
                    Excluded
                }
                Ok(_) => Applies,
                Err(error) => Unknown(error),
            }
        }
    }

    pub(crate) fn verify_evidence<'b>(
        &self,
        read: impl FnOnce(Id) -> Result<&'b Document>,
    ) -> Result<Option<Id>> {
        if self.withdrawal {
            ensure!(
                !self.notice.0.field::<String>("reason")?.trim().is_empty(),
                "missing notice reason"
            );
            Ok(None)
        } else {
            self.notice.calibration_evidence(read).map(Some)
        }
    }
}

pub(super) fn require_micrometres(doc: &Document) -> Result<()> {
    // Older records use the profile's implicit micrometre grid.
    ensure!(
        doc.fields
            .get("unit")
            .is_none_or(|unit| unit.as_str() == Some("um")),
        "declared unit disagrees with the micrometre grid"
    );
    Ok(())
}

fn touches(id: Id, aliases: &Equivalence, premises: &BTreeSet<Id>) -> bool {
    aliases.intersects(id, premises)
}

pub(crate) struct CalibrationLimits<'a> {
    notice: Notice<'a>,
    mode: String,
    interval: [i64; 2],
}

impl CalibrationLimits<'_> {
    pub(crate) fn excludes(&self, document: &Document) -> bool {
        let measurement = MeasurementUse::from_reading(document, WithdrawalRules::Scoped);
        !self.notice.in_scope(&document.scope)
            || measurement.outside_mode(&self.mode)
            || document
                .fields
                .get("mode")
                .is_some_and(|value| !value.is_string())
            || measurement.outside_interval(self.interval)
    }
}

/// For each notice and premise, whether every checked use excluded the withdrawal.
/// An absent entry or any unknown applicability leaves the dependency open.
#[derive(Default)]
pub(crate) struct WithdrawalChecks(BTreeMap<Id, BTreeMap<Id, bool>>);

impl WithdrawalChecks {
    pub(super) fn record(
        &mut self,
        notice: Id,
        targets: &[Id],
        applicability: &WithdrawalApplicability,
    ) {
        let excluded = matches!(applicability, WithdrawalApplicability::Excluded);
        let premises = self.0.entry(notice).or_default();
        for &target in targets {
            premises
                .entry(target)
                .and_modify(|previous| *previous &= excluded)
                .or_insert(excluded);
        }
    }

    pub(super) fn excludes_notice(&self, notices: &BTreeSet<Id>) -> bool {
        let mut premises = notices
            .iter()
            .filter_map(|id| self.0.get(id))
            .flat_map(BTreeMap::values)
            .peekable();
        premises.peek().is_some() && premises.all(|&excluded| excluded)
    }

    pub(crate) fn excludes_target(&self, notice: Id, target: Id) -> bool {
        self.0
            .get(&notice)
            .and_then(|premises| premises.get(&target))
            .copied()
            .unwrap_or(false)
    }
}
