use super::{Evaluation, Receipt, Standing, Status, WithdrawalRules};
use crate::{
    profile::{Document, Kind},
    wire::Id,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

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

    pub(crate) fn within_withdrawal(self, notice: &Document, first_scoped: bool) -> Result<bool> {
        let mode = notice
            .fields
            .get("mode")
            .map(|value| {
                value
                    .as_str()
                    .filter(|mode| !mode.is_empty())
                    .context("invalid withdrawal mode")
            })
            .transpose();
        let interval = notice
            .fields
            .get("interval")
            .map(|_| {
                let [first, last] = notice.field::<[i64; 2]>("interval")?;
                ensure!(first <= last, "reversed withdrawal interval");
                Ok(first..=last)
            })
            .transpose();

        match (mode, interval) {
            (Err(error), _) | (_, Err(error)) if first_scoped => Err(error),
            (Ok(Some(required)), _) if self.mode.is_some_and(|mode| mode != required) => Ok(false),
            (_, Ok(Some(bounds))) if self.event.is_some_and(|event| !bounds.contains(&event)) => {
                Ok(false)
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(mode), Ok(interval)) => {
                ensure!(
                    mode.is_none() || self.mode.is_some(),
                    "withdrawal needs a measurement mode"
                );
                ensure!(
                    interval.is_none() || self.event.is_some(),
                    "withdrawal needs a measurement event"
                );
                Ok(true)
            }
        }
    }
}

impl Evaluation<'_> {
    fn applicable(
        &self,
        target: Id,
        mode: &str,
        event: i64,
        scope: &str,
        out: &mut Receipt,
    ) -> Result<Vec<(Id, i64)>> {
        let targets = self.aliases.endpoints(target);
        let mut branches = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut correction_applies = false;
        for &id in &self.inventory.notices {
            let n = &self.inventory.documents[&id];
            out.visible_notices.insert(id);
            if !n.roles("calibration").any(|k| targets.contains(&k)) {
                continue;
            }
            let [first, last]: [i64; 2] = n.field("interval")?;
            if n.scope != scope
                || n.field::<String>("mode")? != mode
                || event < first
                || event > last
            {
                out.check(
                    "notice applicability",
                    Status::NotApplicable,
                    "outside effective scope, mode or event interval",
                    &[id],
                );
                continue;
            }
            let sig = match self.signed(id, &n.scope, "calibration") {
                Ok(x) => x,
                Err(e) => {
                    out.check("notice authority", Status::Fail, &e.to_string(), &[id]);
                    continue;
                }
            };
            let report = n.one("evidence")?;
            let evidence = self.doc(report, Kind::Occurrence)?;
            require_micrometres(n)?;
            require_micrometres(evidence)?;
            ensure!(
                evidence.scope == n.scope
                    && evidence.field::<i64>("bias")? == n.field::<i64>("bias")?,
                "notice evidence disagrees"
            );
            out.applicable_notices.insert(id);
            out.check(
                "notice authority",
                Status::Pass,
                "verified Ed25519 signature and matching laboratory evidence",
                &[id, sig, report],
            );
            correction_applies = true;
            // A correction is discovered at the notice horizon, outside the original argument.
            // Its warrant includes the laboratory evidence and the attestation actually used.
            let premises = self
                .store
                .explain(self.s, &[id, sig], "read", self.s.objects.len(), 48);
            out.frontier.extend(premises.frontier.keys());
            if !premises.frontier.is_empty() {
                out.unknown(
                    "calibration correction",
                    "correction has missing or restricted inputs",
                    &[id, report],
                );
                continue;
            }
            if self.challenged(
                scope,
                &premises.visited,
                MeasurementUse {
                    mode: Some(mode),
                    event: Some(event),
                },
                out,
            )? {
                continue;
            }
            let aliases = self.aliases.endpoints(id);
            if seen.is_disjoint(&aliases) {
                branches.insert(id, n.field("bias")?);
            }
            seen.extend(aliases);
        }
        ensure!(
            !correction_applies || !branches.is_empty(),
            "no applicable calibration correction retains its warrant"
        );
        Ok(branches.into_iter().collect())
    }
    pub(super) fn measurement(
        &self,
        c: &Document,
        a: &Document,
        out: &mut Receipt,
    ) -> Result<Vec<Standing>> {
        let plan = self.doc(a.one("plan")?, Kind::Plan)?;
        require_micrometres(plan)?;
        let witness = self.doc(a.one("release")?, Kind::Occurrence)?;
        ensure!(
            plan.subject == c.subject
                && plan.scope == c.scope
                && witness.subject == c.subject
                && witness.scope == c.scope
                && witness.one("plan")? == a.one("plan")?
                && witness.field::<u64>("sealed")? < witness.field::<u64>("released")?
                && witness.field::<bool>("concealed")?,
            "plan or controlled release mismatch"
        );
        ensure!(
            self.policy.field::<String>("acquisition_basis")? == "admitted-custody",
            "no acquisition trust basis"
        );
        let permitted = [
            "plan",
            "release",
            "calibration",
            "guarantee",
            "claim",
            "reading",
            "stability",
        ];
        ensure!(
            a.inputs
                .iter()
                .all(|l| permitted.contains(&l.role.as_str()) && l.qualifier.is_empty()),
            "unknown required measurement role or qualifier"
        );
        let pred: String = c.field("predicate")?;
        let spec = Predicate::from_claim(c)?;
        ensure!(
            plan.fields.get("threshold") == c.fields.get("threshold")
                && plan.fields.get("band") == c.fields.get("band"),
            "claim and registered rule differ"
        );
        let cal_id = a.one("calibration")?;
        let cal = self.doc(cal_id, Kind::Calibration)?;
        require_micrometres(cal)?;
        let mode: String = cal.field("mode")?;
        let event: i64 = a.field("event")?;
        let [from, to]: [i64; 2] = cal.field("interval")?;
        ensure!(
            cal.scope == c.scope && from <= event && event <= to,
            "calibration out of scope"
        );
        let g = self.doc(a.one("guarantee")?, Kind::Guarantee)?;
        ensure!(
            g.subject == c.subject
                && g.scope == c.scope
                && g.field::<String>("type")? == "simultaneous-absolute"
                && g.one("calibration")? == cal_id,
            "missing joint-bound warrant"
        );
        let radius: i64 = g.field("radius")?;
        ensure!(radius >= 0, "negative error bound");
        let radius_um = match g.field::<String>("unit")?.as_str() {
            "um" => i128::from(radius),
            "mm" => i128::from(radius) * 1000,
            unit => bail!("unsupported guarantee unit: {unit}"),
        };
        let mut changes = Vec::new();
        let mut gauges = BTreeSet::new();
        for id in a.roles("reading") {
            let r = self.doc(id, Kind::Occurrence)?;
            ensure!(
                r.subject == c.subject
                    && r.scope == c.scope
                    && r.one("calibration")? == cal_id
                    && r.field::<String>("mode")? == mode
                    && r.field::<i64>("event")? == event
                    && r.field::<String>("unit")? == "um"
                    && gauges.insert(r.field::<String>("gauge")?),
                "incompatible or duplicated gauge"
            );
            let [before, after]: [i64; 2] = r.field("positions")?;
            changes.push(i128::from(after) - i128::from(before));
        }
        ensure!(changes.len() >= 2, "missing gauge");
        let stability = self.doc(a.one("stability")?, Kind::Occurrence)?;
        ensure!(
            stability.subject == c.subject
                && stability.scope == c.scope
                && stability.field::<bool>("stable")?,
            "missing stability evidence"
        );
        out.check(
            "acquisition policy",
            Status::Pass,
            "accepted local custody provenance under the pinned policy",
            &[a.one("release")?],
        );
        out.check("scope and exposure",Status::Pass,"same displacement, shared calibration, simultaneous absolute guarantees and controlled release",&[cal_id,a.one("plan")?,a.one("release")?,a.one("guarantee")?,a.one("stability")?]);
        let corrected = self.applicable(cal_id, &mode, event, &c.scope, out)?;
        let mut biases: Vec<_> = corrected.into_iter().map(|(id, b)| (Some(id), b)).collect();
        if biases.is_empty() {
            biases.push((None, cal.field("bias")?))
        }
        let mut results = vec![];
        for (notice, bias) in biases {
            let mut intervals = vec![];
            for &d in &changes {
                let corrected = d - i128::from(bias);
                intervals.push([
                    i64::try_from(corrected - radius_um)?,
                    i64::try_from(corrected + radius_um)?,
                ]);
            }
            let mut hull = intervals[0];
            let mut joint = hull;
            for &[l, h] in &intervals[1..] {
                hull = [hull[0].min(l), hull[1].max(h)];
                joint = [joint[0].max(l), joint[1].min(h)];
            }
            let score = spec.score(hull);
            let standing = spec.claim(joint);
            out.branches.push(json!({
                "notice": notice,
                "bias": bias,
                "mode": mode,
                "event": event,
                "intervals": intervals,
                "hull": hull,
                "joint": joint,
                "score": score,
                "physical": standing,
            }));
            out.check(
                "arithmetic replay",
                Status::Pass,
                "integer micrometres; subtract differential bias once",
                &a.roles("reading").collect::<Vec<_>>(),
            );
            if pred.starts_with("score-") {
                out.check(&pred, score, "frozen hull rule", &[a.one("plan")?]);
            } else {
                if joint[0] > joint[1] {
                    out.unknown(
                        "joint consistency",
                        "incompatible guaranteed intervals",
                        &[a.one("guarantee")?],
                    );
                }
                results.push(standing)
            }
        }
        Ok(results)
    }
}

// Older claims and bias records use the research profile's implicit micrometre grid.
fn require_micrometres(doc: &Document) -> Result<()> {
    ensure!(
        doc.fields
            .get("unit")
            .is_none_or(|unit| unit.as_str() == Some("um")),
        "declared unit disagrees with the micrometre grid"
    );
    Ok(())
}

pub(super) enum Predicate {
    Above(i64),
    Within([i64; 2]),
}
impl Predicate {
    pub(super) fn from_claim(c: &Document) -> Result<Self> {
        require_micrometres(c)?;
        Ok(match c.field::<String>("predicate")?.as_str() {
            "above" | "score-above" => Self::Above(c.field("threshold")?),
            "within" | "score-within" => {
                let [a, b] = c.field("band")?;
                ensure!(a <= b, "reversed forecast band");
                Self::Within([a, b])
            }
            _ => bail!("unknown measurement predicate"),
        })
    }
    pub(super) fn score(&self, [l, h]: [i64; 2]) -> Status {
        let pass = match self {
            Self::Above(t) => l > *t,
            Self::Within([a, b]) => l >= *a && h <= *b,
        };
        if pass { Status::Pass } else { Status::Fail }
    }
    fn claim(&self, interval: [i64; 2]) -> Standing {
        let [l, h] = interval;
        if l > h {
            return Standing::Unresolved;
        }
        if self.score(interval) == Status::Pass {
            return Standing::Supported;
        }
        match self {
            Self::Above(t) if h <= *t => Standing::Refuted,
            Self::Within([a, b]) if h < *a || l > *b => Standing::Refuted,
            _ => Standing::Unresolved,
        }
    }
}
