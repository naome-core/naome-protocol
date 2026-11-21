mod measurement;
mod science;
use measurement::{MeasurementUse, Predicate};

use crate::{
    migration::Equivalence,
    profile::{Document, Kind, PUBLIC, link, verify_attestation},
    store::{Archive, Snapshot},
    wire::{Format, Id},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const V0_1_0_EVALUATOR_ID: &str = "rep-rust-0.1.0";
const FIRST_SCOPED_EVALUATOR_ID: &str = "rep-rust-0.1.2-dev";
const WITHDRAWAL_APPLICABILITY: &str = "withdrawal applicability";
pub(crate) const EVALUATOR_ID: &str = concat!("rep-rust-", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    Supported,
    Refuted,
    Unresolved,
    Contested,
}
impl Standing {
    fn both(pro: bool, con: bool) -> Self {
        match (pro, con) {
            (true, true) => Self::Contested,
            (true, false) => Self::Supported,
            (false, true) => Self::Refuted,
            _ => Self::Unresolved,
        }
    }
}
pub fn default_byte_limit() -> usize {
    64 * 1024 * 1024
}
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub target: Id,
    pub arguments: Vec<Id>,
    pub policy: Id,
    pub budget: usize,
    #[serde(default = "default_byte_limit")]
    pub byte_limit: usize,
    #[serde(default)]
    pub mappings: Vec<Id>,
    #[serde(default)]
    pub previous_receipts: Vec<Id>,
    #[serde(default)]
    pub required_notices: Vec<Id>,
    #[serde(default)]
    pub current_at: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Check {
    pub test: String,
    pub status: Status,
    pub basis: String,
    pub inputs: Vec<Id>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Receipt {
    pub checks: Vec<Check>,
    pub result: Option<Standing>,
    pub horizon_complete: bool,
    pub branches: Vec<Value>,
    pub obligations: BTreeSet<String>,
    pub dependencies: BTreeSet<Id>,
    pub visible_notices: BTreeSet<Id>,
    pub applicable_notices: BTreeSet<Id>,
    pub frontier: BTreeSet<Id>,
}
impl Receipt {
    pub(crate) fn excludes_withdrawal_for(&self, notice: Id, target: Id) -> bool {
        let mut checks = self
            .checks
            .iter()
            .filter(|check| {
                check.test == WITHDRAWAL_APPLICABILITY
                    && check.inputs.first() == Some(&notice)
                    && check
                        .inputs
                        .get(2..)
                        .is_some_and(|targets| targets.contains(&target))
            })
            .peekable();
        checks.peek().is_some() && checks.all(|check| check.status == Status::NotApplicable)
    }

    pub(crate) fn check(&mut self, name: &str, status: Status, basis: &str, ids: &[Id]) {
        self.dependencies.extend(ids);
        self.checks.push(Check {
            test: name.into(),
            status,
            basis: basis.into(),
            inputs: ids.into(),
        });
    }
    pub(crate) fn unknown(&mut self, name: &str, reason: &str, ids: &[Id]) {
        self.check(name, Status::Unknown, reason, ids);
        self.obligations.insert(reason.into());
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum WithdrawalRules {
    ScopeOnly,
    FirstScoped,
    Scoped,
}

pub struct Evaluation<'a> {
    store: &'a Archive,
    s: &'a Snapshot,
    docs: BTreeMap<Id, Document>,
    aliases: Equivalence,
    policy: Document,
    withdrawal_rules: WithdrawalRules,
}
impl Evaluation<'_> {
    fn doc(&self, id: Id, kind: Kind) -> Result<&Document> {
        let d = self
            .docs
            .get(&id)
            .context("missing, opaque or restricted record")?;
        ensure!(d.kind == kind, "wrong input kind");
        Ok(d)
    }
    fn signed(&self, id: Id, scope: &str, role: &str) -> Result<Id> {
        let keys: BTreeMap<String, BTreeSet<String>> = self.policy.field("keys")?;
        let trusted = keys.get(role).context("no admitted authority for role")?;
        for endpoint in self.aliases.endpoints(id) {
            if self.store.resolve(self.s, endpoint, "read").is_err() {
                continue;
            }
            for (&sig, d) in &self.docs {
                if d.kind == Kind::Attestation
                    && verify_attestation(d, endpoint, scope, role, "research-policy-1", trusted)
                        .is_ok()
                {
                    return Ok(sig);
                }
            }
        }
        bail!("no admissible {role} signature")
    }
    fn challenged(
        &self,
        scope: &str,
        premises: &BTreeSet<Id>,
        measurement: MeasurementUse<'_>,
        out: &mut Receipt,
    ) -> Result<bool> {
        let mut open = false;
        for (&id, n) in &self.docs {
            if n.kind != Kind::Notice || n.scope != scope {
                continue;
            }
            let targets: Vec<_> = n
                .roles("target")
                .filter(|&target| !self.aliases.endpoints(target).is_disjoint(premises))
                .collect();
            if targets.is_empty() {
                continue;
            }
            let Ok(signature) = self.signed(id, &n.scope, "notice") else {
                out.check(
                    "notice authority",
                    Status::Fail,
                    "no admitted challenge issuer",
                    &[id],
                );
                continue;
            };
            let reason: String = n.field("reason")?;
            ensure!(!reason.is_empty(), "missing notice reason");
            let action: String = n.field("action")?;
            if self.withdrawal_rules != WithdrawalRules::ScopeOnly && action == "withdraw" {
                let first_scoped = self.withdrawal_rules == WithdrawalRules::FirstScoped;
                let mut inputs = vec![id, signature];
                if !first_scoped {
                    // Identify the targets checked for this use, including when only some
                    // targets of a notice occur in the warrant.
                    inputs.extend(targets);
                }
                match measurement.within_withdrawal(n, first_scoped) {
                    Ok(true) => {}
                    Ok(false) => {
                        out.check(
                            WITHDRAWAL_APPLICABILITY,
                            Status::NotApplicable,
                            "measurement outside the withdrawn mode or event interval",
                            &inputs,
                        );
                        continue;
                    }
                    Err(error) => {
                        out.unknown(WITHDRAWAL_APPLICABILITY, &error.to_string(), &inputs);
                        open = true;
                        continue;
                    }
                }
            }
            out.applicable_notices.insert(id);
            out.check("scoped notice", Status::Pass, &reason, &[id, signature]);
            ensure!(
                matches!(action.as_str(), "withdraw" | "challenge" | "replace"),
                "unrecognised notice action"
            );
            out.obligations.insert(format!(
                "{action} requires a fresh warrant for the affected argument"
            ));
            open = true;
        }
        Ok(open)
    }
    fn signed_tests(&self, c: &Document, a: &Document, out: &mut Receipt) -> Result<Vec<Standing>> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Rule {
            predicate: String,
            polarity: String,
            required: BTreeSet<String>,
            scientific: BTreeSet<String>,
            exemptions: BTreeMap<String, String>,
            role: String,
        }
        let rules: BTreeMap<String, Rule> = self.policy.field("rules")?;
        let name = a.field::<String>("rule")?;
        let rule = rules.get(&name).context("unrecognised warrant")?;
        ensure!(
            rule.predicate == c.field::<String>("predicate")?
                && !rule.scientific.is_empty()
                && ["pro", "con"].contains(&rule.polarity.as_str()),
            "inapplicable warrant policy"
        );
        let mut outcomes = BTreeMap::new();
        for id in a.roles("test") {
            let d = self.doc(id, Kind::Assessment)?;
            ensure!(
                d.scope == c.scope
                    && d.subject == c.subject
                    && d.one("claim")? == a.one("claim")?
                    && d.field::<String>("rule")? == name,
                "delegated test scope mismatch"
            );
            let sig = self.signed(id, &d.scope, &rule.role)?;
            let test: String = d.field("test")?;
            let status: Status = d.field("status")?;
            ensure!(
                outcomes.insert(test.clone(), status).is_none(),
                "ambiguous test inputs"
            );
            out.check(
                &test,
                status,
                "accepted signed check under pinned warrant policy",
                &[id, sig],
            );
        }
        let mut complete = true;
        for test in &rule.required {
            match outcomes.get(test) {
                Some(Status::Pass) => (),
                Some(Status::NotApplicable)
                    if rule
                        .exemptions
                        .get(test)
                        .is_some_and(|e| outcomes.get(e) == Some(&Status::Pass)) => {}
                Some(Status::Fail) => return Ok(vec![]),
                _ => {
                    out.unknown(
                        test,
                        "required check or exemption outstanding",
                        &[a.one("claim")?],
                    );
                    complete = false;
                }
            }
        }
        let applicable = rule
            .scientific
            .iter()
            .any(|test| outcomes.get(test) == Some(&Status::Pass));
        if !applicable {
            out.unknown(
                "scientific warrant",
                "no applicable scientific test passed",
                &[a.one("claim")?],
            );
        }
        Ok(if complete && applicable {
            vec![if rule.polarity == "pro" {
                Standing::Supported
            } else {
                Standing::Refuted
            }]
        } else {
            vec![]
        })
    }
    fn warrant(
        &self,
        c: &Document,
        a: &Document,
        id: Id,
        out: &mut Receipt,
    ) -> Result<Vec<Standing>> {
        match a.field::<String>("method")?.as_str() {
            "simultaneous-bounds" => self.measurement(c, a, out),
            "signed-tests" => self.signed_tests(c, a, out),
            "typed-outcome" => {
                ensure!(
                    c.field::<String>("predicate")? == "training-target",
                    "wrong predicate for a typed outcome"
                );
                let profile_id = c.one("target-profile")?;
                let profile = self.doc(profile_id, Kind::Profile)?;
                let outcome_id = a.one("outcome")?;
                let outcome = self.doc(outcome_id, Kind::Occurrence)?;
                ensure!(
                    outcome.subject == c.subject
                        && outcome.scope == c.scope
                        && outcome.one("target-profile")? == profile_id,
                    "typed outcome differs in subject, scope or profile"
                );
                let target: crate::training_target::TrainingTarget = outcome.field("target")?;
                target.check(profile, outcome)?;
                let signature = self.signed(outcome_id, &c.scope, "outcome")?;
                out.check(
                    "typed outcome",
                    Status::Pass,
                    "profile-checked target attested by an admitted issuer",
                    &[id, profile_id, outcome_id, signature],
                );
                out.branches.push(json!({"training_target":target}));
                Ok(vec![Standing::Supported])
            }
            "certified-hull" => {
                let p_id = a.one("projection")?;
                let p = self.doc(p_id, Kind::Projection)?;
                let sig = self.signed(p_id, &p.scope, "projection")?;
                ensure!(
                    p.scope == c.scope
                        && p.subject == c.subject
                        && p.field::<String>("unit")? == "um"
                        && p.fields.get("threshold") == c.fields.get("threshold")
                        && p.fields.get("band") == c.fields.get("band"),
                    "projection scope or scoring rule mismatch"
                );
                let hull = p.field::<[i64; 2]>("hull")?;
                ensure!(hull[0] <= hull[1], "empty certified hull");
                let score = Predicate::from_claim(c)?.score(hull);
                out.branches.push(json!({"hull":hull,"score":score}));
                if c.field::<String>("predicate")?.starts_with("score-") {
                    out.check(
                        "certified hull score",
                        score,
                        "verified scoped projection",
                        &[p_id, sig],
                    );
                } else {
                    out.unknown(
                        "physical warrant",
                        "missing joint-bound warrant",
                        &[p_id, sig],
                    );
                }
                Ok(vec![])
            }
            "delegated" => {
                let original = a.one("receipt")?;
                let r = self.doc(original, Kind::Assessment)?;
                ensure!(
                    r.subject == c.subject
                        && r.scope == c.scope
                        && r.one("claim")? == a.one("claim")?
                        && r.field::<String>("predicate")? == c.field::<String>("predicate")?,
                    "delegated receipt target mismatch"
                );
                let sig = self.signed(original, &c.scope, "assessor")?;
                out.check(
                    "delegated result",
                    Status::Pass,
                    "accepted signed assessment under local role policy",
                    &[id, original, sig],
                );
                let horizon = r.one("snapshot")?;
                let original_view: Snapshot =
                    serde_json::from_slice(&self.store.resolve(self.s, horizon, "read")?)?;
                ensure!(
                    original_view.objects.is_subset(&self.s.objects),
                    "delegated inventory is unavailable"
                );
                let lineage =
                    self.store
                        .explain(self.s, &[original], "read", self.s.objects.len(), 48);
                ensure!(
                    lineage.frontier.is_empty(),
                    "delegated dependencies are incomplete"
                );
                for (notice_id, n) in &self.docs {
                    if n.kind == Kind::Notice
                        && !original_view.objects.contains(notice_id)
                        && n.inputs.iter().any(|l| {
                            self.aliases
                                .endpoints(l.target)
                                .iter()
                                .any(|x| lineage.visited.contains(x))
                        })
                    {
                        bail!("delegated receipt requires reassessment after a later notice");
                    }
                }
                Ok(vec![r.field("result")?])
            }
            method => science::evaluate(
                method,
                c,
                a,
                |id| {
                    self.docs
                        .get(&id)
                        .cloned()
                        .context("missing scientific input")
                },
                out,
            ),
        }
    }
}
pub fn assess(store: &Archive, s: &Snapshot, q: &Query) -> Result<Receipt> {
    assess_for_evaluator(store, s, q, EVALUATOR_ID)
}

pub(crate) fn assess_for_evaluator(
    store: &Archive,
    s: &Snapshot,
    q: &Query,
    evaluator: &str,
) -> Result<Receipt> {
    let withdrawal_rules = match evaluator {
        EVALUATOR_ID => WithdrawalRules::Scoped,
        FIRST_SCOPED_EVALUATOR_ID => WithdrawalRules::FirstScoped,
        V0_1_0_EVALUATOR_ID | "rep-rust-0.1.1" => WithdrawalRules::ScopeOnly,
        _ => bail!("unsupported receipt evaluator"),
    };
    let mut out = Receipt::default();
    let target = store.read(s, q.target, "read");
    let score = target
        .as_ref()
        .ok()
        .and_then(|c| c.field::<String>("predicate").ok())
        .is_some_and(|x| x.starts_with("score-"));
    out.result = if score {
        None
    } else {
        Some(Standing::Unresolved)
    };
    let run = (|| -> Result<()> {
        let c = target?;
        ensure!(c.kind == Kind::Claim, "query target is not a claim");
        let policy = store.read(s, q.policy, "read")?;
        ensure!(
            policy.kind == Kind::Policy && policy.scope == "research-policy-1",
            "unsupported query policy"
        );
        out.dependencies.extend([q.target, q.policy]);
        let mut docs = BTreeMap::new();
        let mut hidden_notices = false;
        let mut bytes_left = q.byte_limit;
        for (i, &id) in s.objects.iter().enumerate() {
            if i >= q.budget {
                out.frontier.insert(id);
                continue;
            }
            if id.format().is_some() {
                let size = store.size(id)?;
                if size > bytes_left {
                    out.frontier.insert(id);
                    continue;
                }
                bytes_left -= size;
                match store.read(s, id, "read") {
                    Ok(d) => {
                        if d.kind == Kind::Notice {
                            out.visible_notices.insert(id);
                        }
                        docs.insert(id, d);
                    }
                    Err(_) if store.may_be_notice(s, id) => {
                        out.frontier.insert(id);
                        hidden_notices = true;
                    }
                    Err(_) => {}
                }
            }
        }
        ensure!(
            !hidden_notices,
            "notice horizon contains inaccessible or opaque records"
        );
        ensure!(out.frontier.is_empty(), "notice search budget exhausted");
        let mut aliases = Equivalence::default();
        for &mapping in &q.mappings {
            aliases.add(store.verify_mapping(s, mapping)?);
            out.dependencies.insert(mapping);
        }
        for &id in &q.previous_receipts {
            let prior = store.read(s, id, "read")?;
            ensure!(
                prior.kind == Kind::Assessment
                    && aliases.endpoints(prior.one("claim")?).contains(&q.target),
                "previous receipt covers another target"
            );
        }
        let view = Evaluation {
            store,
            s,
            docs,
            aliases,
            policy,
            withdrawal_rules,
        };
        let mut pro = false;
        let mut con = false;
        for &arg in &q.arguments {
            let evidence = store.explain(s, &[arg], "read", q.budget, 48);
            // Preserve the archival provenance used to replay historical receipts.
            out.dependencies.extend(&evidence.visited);
            let argument = view.doc(arg, Kind::Argument);
            let recorded_reading = argument
                .as_ref()
                .ok()
                .filter(|a| {
                    a.fields.get("method").and_then(Value::as_str) == Some("recorded-difference")
                })
                .and_then(|a| a.one("reading").ok());
            let premises = match recorded_reading {
                Some(reading) => store.explain_recorded_reading(s, arg, reading, q.budget),
                None => evidence,
            };
            out.frontier.extend(premises.frontier.keys());
            let result = (|| -> Result<Vec<Standing>> {
                ensure!(
                    premises.frontier.is_empty(),
                    "argument has missing or restricted inputs"
                );
                let a = argument?;
                ensure!(
                    a.subject == c.subject && a.scope == c.scope && a.one("claim")? == q.target,
                    "argument targets a different claim"
                );
                // Both measurement warrants read this occurrence. Simultaneous bounds
                // also verifies that every gauge agrees on mode and event.
                let reading = match a.fields.get("method").and_then(Value::as_str) {
                    Some("recorded-difference" | "simultaneous-bounds") => {
                        a.roles("reading").next().and_then(|id| view.docs.get(&id))
                    }
                    _ => None,
                };
                let measurement = reading
                    .map(|reading| MeasurementUse::from_reading(reading, withdrawal_rules))
                    .unwrap_or_default();
                ensure!(
                    !view.challenged(&c.scope, &premises.visited, measurement, &mut out)?,
                    "argument under an admitted notice"
                );
                view.warrant(&c, a, arg, &mut out)
            })();
            match result {
                Ok(results) => {
                    for r in results {
                        pro |= matches!(r, Standing::Supported | Standing::Contested);
                        con |= matches!(r, Standing::Refuted | Standing::Contested);
                    }
                }
                Err(e) => out.unknown("argument warrant", &e.to_string(), &[arg]),
            }
        }
        for &notice in &q.required_notices {
            ensure!(
                out.applicable_notices
                    .iter()
                    .any(|n| view.aliases.endpoints(*n).contains(&notice)),
                "required notice applicability unknown"
            );
        }
        if let Some(now) = q.current_at {
            let max_age: u64 = view.policy.field("max_age")?;
            let sources: Vec<String> = view.policy.field("required_sources")?;
            ensure!(!sources.is_empty(), "no current authority named");
            for source in sources {
                let mut fresh = false;
                for (&id, d) in &view.docs {
                    if d.kind == Kind::Occurrence
                        && d.subject == source
                        && d.scope == "notice-horizon"
                        && let Ok(time) = d.field::<u64>("time")
                        && time <= now
                        && now - time <= max_age
                        && let Ok(h) = d.one("inventory")
                        && let Ok(b) = store.resolve(s, h, "read")
                        && let Ok(inventory) = serde_json::from_slice::<Snapshot>(&b)
                        && inventory.source == source
                        && inventory.objects.is_subset(&s.objects)
                        && let Ok(sig) = view.signed(id, "notice-horizon", "horizon")
                    {
                        out.check(
                            "notice freshness",
                            Status::Pass,
                            "signed, available authority inventory within permitted age",
                            &[id, h, sig],
                        );
                        fresh = true;
                        break;
                    }
                }
                ensure!(
                    fresh,
                    "required notice authority unavailable or stale: {source}"
                )
            }
        }
        out.horizon_complete = true;
        if !score {
            out.result = Some(Standing::both(pro, con));
            if pro && con {
                out.obligations
                    .insert("adjudicate competing admissible accounts".into());
            }
            if !pro && !con {
                out.obligations
                    .insert("no decisive admitted argument".into());
            }
        }
        Ok(())
    })();
    if let Err(e) = run {
        out.unknown("query completeness", &e.to_string(), &[q.target, q.policy]);
    }
    Ok(out)
}
pub fn deposit_receipt(
    store: &Archive,
    s: &Snapshot,
    q: &Query,
    r: &Receipt,
    f: Format,
) -> Result<Id> {
    deposit_receipt_for_evaluator(store, s, q, r, f, EVALUATOR_ID)
}
pub(crate) fn deposit_receipt_for_evaluator(
    store: &Archive,
    s: &Snapshot,
    q: &Query,
    r: &Receipt,
    f: Format,
    evaluator: &str,
) -> Result<Id> {
    store.blob(&serde_json::to_vec(s)?, Id::blob(PUBLIC))?;
    store.document(
        &receipt_document(store, s, q, r, evaluator)?,
        f,
        Id::blob(PUBLIC),
    )
}
pub(crate) fn receipt_document(
    store: &Archive,
    s: &Snapshot,
    q: &Query,
    r: &Receipt,
    evaluator: &str,
) -> Result<Document> {
    let head = s.id()?;
    let mut edges = vec![
        link("claim", q.target),
        link("policy", q.policy),
        link("snapshot", head),
    ];
    edges.extend(
        q.arguments
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|x| link("argument", x)),
    );
    edges.extend(r.dependencies.iter().map(|x| link("dependency", *x)));
    edges.extend(r.visible_notices.iter().map(|x| link("visible-notice", *x)));
    edges.extend(
        q.previous_receipts
            .iter()
            .map(|id| link("previous-receipt", *id)),
    );
    edges.extend(q.mappings.iter().map(|x| link("mapping", *x)));
    edges.extend(
        q.required_notices
            .iter()
            .map(|x| link("required-notice", *x)),
    );
    let claim = store.read(s, q.target, "read").ok();
    let subject = claim.as_ref().map_or("opaque target", |c| &c.subject);
    let scope = claim
        .as_ref()
        .map_or("unresolved interpretation", |c| &c.scope);
    let fields = json!({
        "predicate": claim.as_ref().and_then(|c|c.field::<String>("predicate").ok()).unwrap_or_else(||"uninterpreted".into()),
        "result": r.result,
        "original_transcript": r,
        "original_query": q,
        "evaluator": evaluator,
    });
    Document::new(Kind::Assessment, subject, scope, fields, edges)
}
