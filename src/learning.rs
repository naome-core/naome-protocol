mod prospective;

pub use crate::evaluation::Impact;
use crate::{
    evaluation::{
        Assessment, EVALUATOR_ID, EvidenceView, ReadInventory, assess_with_inventory,
        correction::Notice,
        dependency_correction::{
            CorrectionImpact, CorrectionKeys, DependencyCorrection, admitted_corrections,
            correction_keys,
        },
    },
    migration::Equivalence,
    profile::{Document, Kind, PUBLIC, link, verify_attestation},
    store::{Archive, ManifestEncoding, Snapshot},
    wire::{Format, Id},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub features: Vec<Id>,
    pub label: Id,
    pub partition: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingRequest {
    pub feature_cutoff: Id,
    #[serde(default)]
    pub forecast_plan: Option<Id>,
    #[serde(default)]
    pub mappings: Vec<Id>,
    pub label_head: Id,
    /// The recipient independently admits this policy.
    pub policy: Id,
    #[serde(default)]
    pub standing: LabelStanding,
    #[serde(default)]
    pub project: Option<String>,
    pub examples: Vec<Example>,
    pub independent_roles: BTreeSet<String>,
    pub binary: bool,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelStanding {
    #[default]
    Current,
    Historical,
}

#[derive(Debug, Serialize)]
pub struct Extraction {
    pub manifest: Id,
    pub included: Vec<Id>,
    pub withheld: BTreeMap<Id, String>,
    /// Decisions corresponding positionally to the request examples.
    pub rows: Vec<TrainingRow>,
}

#[derive(Debug, Serialize)]
pub struct TrainingRow {
    pub label: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
}

fn label_value(
    receipt: &crate::evaluation::Receipt,
    check: Option<&str>,
    condition: Option<Id>,
) -> Result<Value> {
    if !receipt.horizon_complete {
        return Ok(json!("unknown"));
    }
    let mut values = BTreeSet::new();
    if let Some(name) = check {
        ensure!(
            condition.is_none(),
            "a check label cannot silently select a branch"
        );
        for test in receipt.checks.iter().filter(|test| test.test == name) {
            values.insert(
                serde_json::to_value(test.status)?
                    .as_str()
                    .context("check status")?
                    .to_owned(),
            );
        }
    } else if let Some(id) = condition {
        let key = if receipt.result.is_some() {
            "physical"
        } else {
            "score"
        };
        for branch in &receipt.branches {
            if branch.get("notice") == Some(&json!(id)) {
                values.insert(
                    branch[key]
                        .as_str()
                        .context("missing conditional outcome")?
                        .to_owned(),
                );
            }
        }
        ensure!(!values.is_empty(), "unknown branch condition");
    } else if let Some(standing) = receipt.result {
        if standing == crate::evaluation::Standing::Supported {
            let mut targets = receipt
                .branches
                .iter()
                .filter_map(|branch| branch.get("training_target"));
            if let Some(target) = targets.next() {
                return Ok(if targets.all(|other| other == target) {
                    target.clone()
                } else {
                    json!("contested")
                });
            }
        }
        return Ok(serde_json::to_value(standing)?);
    } else {
        values.extend(
            receipt
                .branches
                .iter()
                .filter_map(|branch| branch["score"].as_str().map(str::to_owned)),
        );
    }
    Ok(match values.len() {
        0 => json!("unknown"),
        1 => json!(values.into_iter().next().context("missing value")?),
        _ => json!("contested"),
    })
}

struct ValidatedExample {
    groups: BTreeSet<(String, Id)>,
    release_attestations: BTreeSet<Id>,
}

struct TrainingTrust {
    keys: BTreeMap<String, BTreeSet<String>>,
    correction_keys: CorrectionKeys,
    inventory: ReadInventory,
}

impl TrainingTrust {
    fn load(store: &Archive, head: &Snapshot, policy: Id, retain_for_replay: bool) -> Result<Self> {
        let policy = store.read(head, policy, "read")?;
        ensure!(
            policy.kind == Kind::Policy && policy.scope == "research-policy-1",
            "unsupported training policy"
        );
        Ok(Self {
            keys: policy.field("keys")?,
            correction_keys: correction_keys(&policy)?,
            inventory: ReadInventory::load(store, head, retain_for_replay)?,
        })
    }

    fn signatures(
        &self,
        aliases: &Equivalence,
        target: Id,
        scope: &str,
        role: &str,
    ) -> BTreeSet<Id> {
        let Some(keys) = self.keys.get(role) else {
            return BTreeSet::new();
        };
        aliases
            .members(target)
            .flat_map(|id| {
                self.inventory
                    .attestations(id)
                    .filter_map(move |(sig, doc)| {
                        verify_attestation(doc, id, scope, role, "research-policy-1", keys)
                            .ok()
                            .map(|()| sig)
                    })
            })
            .collect()
    }

    fn origin(
        &self,
        store: &Archive,
        head: &Snapshot,
        receipt: &Document,
        aliases: &Equivalence,
        origin: &str,
        bound_signatures: Option<&BTreeSet<Id>>,
    ) -> Result<BTreeSet<Id>> {
        ensure!(
            ["observed", "simulated", "synthetic", "inferred"].contains(&origin),
            "unknown example origin"
        );
        let mut signatures = BTreeSet::new();
        for id in receipt.roles("argument") {
            let argument = store.read(head, id, "read")?;
            ensure!(
                argument.kind == Kind::Argument
                    && argument.subject == receipt.subject
                    && argument.scope == receipt.scope
                    && argument.field::<String>("origin")? == origin,
                "requested origin differs from the argument's declared origin"
            );
            let candidates = self.signatures(aliases, id, &argument.scope, "data-origin");
            let signature = candidates
                .into_iter()
                .find(|candidate| {
                    bound_signatures.is_none_or(|bound| aliases.intersects(*candidate, bound))
                })
                .context("missing bound, admitted data-origin attestation")?;
            signatures.insert(signature);
        }
        ensure!(!signatures.is_empty(), "no attestable argument origin");
        Ok(signatures)
    }
}

fn canonical_set(aliases: &Equivalence, ids: &BTreeSet<Id>) -> BTreeSet<Id> {
    ids.iter().map(|id| aliases.representative(*id)).collect()
}

fn optional_link(doc: &Document, role: &str) -> Result<Option<Id>> {
    let mut inputs = doc.roles(role);
    let value = inputs.next();
    ensure!(inputs.next().is_none(), "ambiguous {role}");
    Ok(value)
}

mod replay_cache;

struct TrainingReview<'a> {
    store: &'a Archive,
    before: &'a Snapshot,
    after: &'a Snapshot,
    request: &'a TrainingRequest,
    aliases: &'a Equivalence,
    trust: TrainingTrust,
    requires_legacy_mapping: bool,
    // A bounded working set for the most recently replayed historical inventory.
    historical: Option<(Id, Snapshot, ReadInventory)>,
    previous_horizons: replay_cache::Cache,
    access: &'a crate::profile::TrainingAccess<'a>,
}

impl TrainingReview<'_> {
    fn validate(&mut self, ex: &Example) -> Result<ValidatedExample> {
        let Self {
            store,
            before,
            after,
            request,
            aliases,
            access,
            ..
        } = *self;
        let l = store.read_for_training(after, ex.label, access)?;
        ensure!(l.kind == Kind::Label, "expected label");
        let value = l.fields.get("value").context("missing label value")?;
        let check = l
            .fields
            .get("check")
            .map(|field| field.as_str().context("invalid label check"))
            .transpose()?;
        let condition = optional_link(&l, "condition")?;
        ensure!(
            !request.binary
                || (condition.is_none()
                    && value
                        .as_str()
                        .is_some_and(|v| ["pass", "fail", "supported", "refuted"].contains(&v))),
            "label has no single unconditional binary value"
        );
        ensure!(
            !ex.features.is_empty() && !ex.partition.is_empty(),
            "empty features or partition"
        );
        let receipt_id = l.one("receipt")?;
        let receipt = store.read_for_training(after, receipt_id, access)?;
        let claim = l.one("claim")?;
        ensure!(
            receipt.kind == Kind::Assessment
                && receipt.one("claim")? == claim
                && receipt.one("snapshot")? == l.one("snapshot")?
                && receipt.subject == l.subject
                && receipt.scope == l.scope
                && check
                    .map(str::to_owned)
                    .unwrap_or(receipt.field::<String>("predicate")?)
                    == l.field::<String>("predicate")?,
            "label target or horizon differs from its receipt"
        );
        let original: crate::evaluation::Query = receipt.field("original_query")?;
        ensure!(
            aliases.equivalent(original.policy, request.policy),
            "receipt policy differs from admitted training policy"
        );
        let receipt_head = receipt.one("snapshot")?;
        ensure!(
            original.budget <= 100_000
                && original.byte_limit <= crate::evaluation::default_byte_limit(),
            "label replay exceeds local extraction budget"
        );
        let evaluator: String = receipt.field("evaluator")?;
        if self.requires_legacy_mapping {
            ensure!(
                matches!(
                    crate::evaluation::manifest_encoding(&evaluator)?,
                    ManifestEncoding::LegacyJson
                ),
                "current receipt requires canonical mapping inventories"
            );
        }
        if self
            .historical
            .as_ref()
            .is_none_or(|(head, _, _)| *head != receipt_head)
        {
            let saved = self.previous_horizons.take(receipt_head)?;
            if let Some((id, snapshot, inventory)) = self.historical.take() {
                self.previous_horizons.remember(id, &snapshot, &inventory)?;
            }
            let (snapshot, inventory) = match saved {
                Some(pair) => pair,
                None => (
                    store.receipt_horizon(after, receipt_head)?,
                    ReadInventory::for_replay(),
                ),
            };
            self.historical = Some((receipt_head, snapshot, inventory));
        }
        let (historical_head, historical, inventory) = self
            .historical
            .as_mut()
            .context("missing receipt inventory")?;
        let replay =
            assess_with_inventory(store, historical, &original, &evaluator, inventory)?.receipt;
        let expected = crate::evaluation::receipt_document(
            store,
            historical,
            *historical_head,
            &original,
            &replay,
            &evaluator,
        )?
        .encode(Id::blob(PUBLIC))?;
        ensure!(
            [Format::A, Format::B]
                .iter()
                .any(|f| aliases.equivalent(receipt_id, Id::record(*f, &expected.encode(*f)))),
            "receipt fields or dependencies do not replay"
        );
        ensure!(
            &label_value(&replay, check, condition)? == value,
            "label disagrees with the assessed predicate"
        );
        let bound_signatures = l.roles("origin-attestation").collect::<BTreeSet<_>>();
        let origins = self.trust.origin(
            store,
            after,
            &receipt,
            aliases,
            &l.field::<String>("origin")?,
            Some(&bound_signatures),
        )?;
        ensure!(
            bound_signatures.len() == origins.len(),
            "ambiguous origin attestations"
        );
        let witnesses = prospective::witnesses(self, &receipt, &original.arguments)?;
        let mut axes = BTreeSet::from([("claim".to_owned(), aliases.representative(claim))]);
        let features = store.explain_for_training(before, &ex.features, access);
        let label = store.explain_for_training(after, &[ex.label], access);
        ensure!(
            features.frontier.is_empty() && label.frontier.is_empty(),
            "unavailable, late, or prohibited training input"
        );
        for id in features
            .visited
            .union(&label.visited)
            .copied()
            .filter(|id| id.format().is_some())
        {
            let horizon = if features.visited.contains(&id) {
                before
            } else {
                after
            };
            let doc = store.read_for_training(horizon, id, access)?;
            for edge in doc.inputs {
                if request.independent_roles.contains(&edge.role) {
                    axes.insert((edge.role, aliases.representative(edge.target)));
                }
            }
        }
        if request.standing == LabelStanding::Current {
            let trust = &mut self.trust;
            ensure!(
                trust.inventory.notice_search_complete(),
                "requested notice horizon has unreadable notices or records of unknown kind"
            );
            let mut original = original;
            original.mappings.extend(request.mappings.iter().copied());
            original.mappings.sort_unstable();
            original.mappings.dedup();
            let Assessment {
                receipt: current,
                withdrawals,
            } = assess_with_inventory(store, after, &original, EVALUATOR_ID, &mut trust.inventory)?;
            ensure!(
                current.horizon_complete
                    && current.frontier.is_empty()
                    && label_value(&current, check, condition)? == *value
                    && canonical_set(aliases, &current.applicable_notices)
                        == canonical_set(aliases, &replay.applicable_notices),
                "label requires reassessment at the requested notice horizon"
            );
            let evidence = EvidenceView {
                store,
                snapshot: after,
                inventory: &trust.inventory,
                aliases,
            };
            for admitted in admitted_corrections(&evidence, &trust.correction_keys, true)? {
                ensure!(
                    admitted
                        .correction
                        .exposure(&evidence, &ex.features, &features)?
                        == CorrectionImpact::Outside
                        && admitted
                            .correction
                            .exposure(&evidence, &[ex.label], &label)?
                            == CorrectionImpact::Outside,
                    "an admitted input correction requires reassessment of a training dependency"
                );
            }
            for &id in &trust.inventory.notices {
                let notice = &trust.inventory.documents[&id];
                if current.applicable_notices.contains(&id)
                    || DependencyCorrection::is_correction(notice)
                {
                    continue;
                }
                // A warrant's exclusion applies to the label's use. An
                // independent feature cannot reuse that exclusion.
                let correction = Notice(notice);
                let affects_training = correction
                    .matching_targets(aliases, &features.visited)
                    .next()
                    .is_some()
                    || correction
                        .matching_targets(aliases, &label.visited)
                        .any(|target| !withdrawals.excludes_target(id, target));
                if affects_training
                    && !trust
                        .signatures(aliases, id, &notice.scope, "notice")
                        .is_empty()
                {
                    anyhow::bail!("an admitted notice affects a training dependency");
                }
            }
        }
        for axis in &request.independent_roles {
            ensure!(
                axes.iter().any(|(role, _)| role == axis),
                "unknown independence axis {axis}"
            );
        }
        Ok(ValidatedExample {
            groups: axes,
            release_attestations: witnesses,
        })
    }
}

impl Archive {
    pub fn label(
        &self,
        s: &Snapshot,
        receipt: Id,
        origin: &str,
        check: Option<&str>,
        condition: Option<Id>,
        f: Format,
    ) -> Result<Id> {
        let d = self.read(s, receipt, "read")?;
        ensure!(d.kind == Kind::Assessment, "expected assessment");
        let mut aliases = Equivalence::default();
        for mapping in d.roles("mapping") {
            let evaluator: String = d.field("evaluator")?;
            aliases.add(self.verify_mapping_with(
                s,
                mapping,
                crate::evaluation::manifest_encoding(&evaluator)?,
            )?);
        }
        let trust = TrainingTrust::load(self, s, d.one("policy")?, false)?;
        let origins = trust.origin(self, s, &d, &aliases, origin, None)?;
        let value = label_value(&d.field("original_transcript")?, check, condition)?;
        let predicate = check.map(str::to_owned).unwrap_or(d.field("predicate")?);
        let mut fields = json!({"value":value,"origin":origin,"predicate":predicate});
        if let Some(c) = check {
            fields["check"] = json!(c);
        }
        let mut links = vec![
            link("receipt", receipt),
            link("claim", d.one("claim")?),
            link("snapshot", d.one("snapshot")?),
        ];
        links.extend(origins.into_iter().map(|id| link("origin-attestation", id)));
        if let Some(id) = condition {
            links.push(link("condition", id));
        }
        self.document(
            &Document::new(Kind::Label, &d.subject, &d.scope, fields, links)?,
            f,
            Id::blob(PUBLIC),
        )
    }
    pub fn training_manifest(&self, request: &TrainingRequest, f: Format) -> Result<Extraction> {
        // Hold a stable custody view across cached reads and the manifest write.
        let transaction = self
            .db
            .is_autocommit()
            .then(|| {
                rusqlite::Transaction::new_unchecked(
                    &self.db,
                    rusqlite::TransactionBehavior::Immediate,
                )
            })
            .transpose()?;
        let before = self.head(request.feature_cutoff)?;
        let after = self.head(request.label_head)?;
        ensure!(
            before.objects.is_subset(&after.objects),
            "feature cutoff outside outcome history"
        );
        let mut aliases = Equivalence::default();
        let mut requires_legacy_mapping = false;
        for &id in &request.mappings {
            let pairs = match self.verify_mapping(&after, id) {
                Ok(pairs) => pairs,
                Err(_) if request.standing == LabelStanding::Historical => {
                    let pairs =
                        self.verify_mapping_with(&after, id, ManifestEncoding::LegacyJson)?;
                    requires_legacy_mapping = true;
                    pairs
                }
                Err(error) => return Err(error),
            };
            aliases.add(pairs);
        }
        let trust = TrainingTrust::load(
            self,
            &after,
            request.policy,
            request.standing == LabelStanding::Current,
        )?;
        let access = crate::profile::TrainingAccess::new(request.project.as_deref())?;
        let mut review = TrainingReview {
            store: self,
            before: &before,
            after: &after,
            request,
            aliases: &aliases,
            trust,
            requires_legacy_mapping,
            historical: None,
            previous_horizons: Default::default(),
            access: &access,
        };
        let mut partitions: BTreeMap<(String, Id), String> = BTreeMap::new();
        let mut included = vec![];
        let mut withheld = BTreeMap::new();
        let mut rows = Vec::with_capacity(request.examples.len());
        let mut links = vec![link("policy", request.policy)];
        let mut release_signatures = BTreeSet::new();
        let mut exact_rows = BTreeSet::new();
        for ex in &request.examples {
            let row = review.validate(ex).and_then(|example| {
                ensure!(
                    example
                        .groups
                        .iter()
                        .all(|x| partitions.get(x).is_none_or(|p| p == &ex.partition)),
                    "shared ancestor crosses declared partitions"
                );
                let row_key = (
                    aliases.representative(ex.label),
                    ex.features
                        .iter()
                        .map(|id| aliases.representative(*id))
                        .collect::<Vec<_>>(),
                );
                ensure!(exact_rows.insert(row_key), "duplicate training example");
                Ok(example)
            });
            let rejection = match row {
                Ok(ValidatedExample {
                    groups: axes,
                    release_attestations: witnesses,
                }) => {
                    release_signatures.extend(witnesses);
                    for a in axes {
                        partitions.insert(a, ex.partition.clone());
                    }
                    included.push(ex.label);
                    let mut l = link("label", ex.label);
                    l.qualifier = format!("{}:{}", included.len(), ex.partition);
                    links.push(l);
                    for (i, &id) in ex.features.iter().enumerate() {
                        let mut l = link("feature", id);
                        l.qualifier = format!("{}:{i}", included.len());
                        links.push(l);
                    }
                    None
                }
                Err(error) => {
                    let reason = error.to_string();
                    withheld.insert(ex.label, reason.clone());
                    Some(reason)
                }
            };
            rows.push(TrainingRow {
                label: ex.label,
                rejection,
            });
        }
        for (role, s) in [("feature-cutoff", &before), ("label-horizon", &after)] {
            let id = self.deposit_manifest(s)?;
            links.push(link(role, id));
        }
        if let Some(plan) = request.forecast_plan {
            links.push(link("forecast-plan", plan));
        }
        links.extend(request.mappings.iter().map(|id| link("mapping", *id)));
        links.extend(
            release_signatures
                .into_iter()
                .map(|id| link("release-attestation", id)),
        );
        let d = Document::new(
            Kind::Manifest,
            "training extraction",
            "research-training-1",
            json!({
                "rule": "as-of-feature-and-label",
                "label_standing": request.standing,
                "project": request.project,
                "permission_checked_at": access.checked_at(),
                "split_identity": "claim and declared dependency roles",
                "exposure": if included.is_empty() { "no eligible examples" }
                    else if request.forecast_plan.is_some() { "controlled release" }
                    else { "retrospective or unknown" },
                "binary": request.binary,
                "independent_roles": request.independent_roles,
                "withheld_count": request.examples.len()-included.len(),
                "requested": request.examples.len(),
            }),
            links,
        )?;
        let manifest = self.document(&d, f, Id::blob(PUBLIC))?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(Extraction {
            manifest,
            included,
            withheld,
            rows,
        })
    }
}
