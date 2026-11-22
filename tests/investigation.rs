#![cfg(feature = "cli")]

use anyhow::Result;
use naome_protocol::{
    executor::{Dispatch, Request},
    fixture::{self, put},
    profile::{Kind, PUBLIC, READ_ONLY, link},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempfile::tempdir;

#[test]
fn ambiguous_execution_survives_restart_and_shared_reservations() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("state");
    let a = Archive::open(&path)?;
    a.resource("samples", 3)?;
    let f = Format::A;
    let key = fixture::fixture_key(17);
    let trusted = BTreeSet::from([hex::encode(key.verifying_key().to_bytes())]);
    let auth = put(
        &a,
        f,
        Kind::Authority,
        "sample",
        "lab",
        json!({"actor":"station","operation":"observe","valid":[10,20],"ceilings":{"samples":2}}),
        vec![],
    )?;
    let sig = fixture::sign(&a, f, auth, "lab", "executor", 17)?;
    let decision = |name: &str| {
        put(
            &a,
            f,
            Kind::Decision,
            "sample",
            "lab",
            json!({"operation":"observe","attempt":name}),
            vec![link("authority", auth)],
        )
    };
    let mut req = Request {
        attempt: "1".into(),
        actor: "station".into(),
        subject: "sample".into(),
        operation: "observe".into(),
        decision: decision("1")?,
        authority: auth,
        signature: sig,
        costs: BTreeMap::from([("samples".into(), 1)]),
    };
    let second = decision("2")?;
    let legacy = Request {
        attempt: "legacy".into(),
        decision: decision("legacy")?,
        ..serde_json::from_slice(&serde_json::to_vec(&req)?)?
    };
    let s = a.snapshot("station", BTreeSet::new())?;
    drop(a);
    let old = rusqlite::Connection::open(&path)?;
    old.execute(
        "UPDATE limits SET capacity=capacity-1 WHERE resource='samples'",
        [],
    )?;
    old.execute(
        "INSERT INTO attempts VALUES(?,?,'execution-unknown',NULL)",
        rusqlite::params![legacy.attempt, serde_json::to_vec(&legacy)?],
    )?;
    drop(old);
    let mut a = Archive::open(&path)?;
    assert!(a.reserve(&s, &req, &trusted, 21).is_err());
    // General read access does not grant the public authority required for dispatch.
    let approved = req.decision;
    req.decision = a.document(&a.read(&s, approved, "read")?, f, Id::blob(READ_ONLY))?;
    let restricted = a.snapshot("readable decision", [s.id()?].into())?;
    assert!(a.read(&restricted, req.decision, "read").is_ok());
    assert!(a.reserve(&restricted, &req, &trusted, 12).is_err());
    req.decision = approved;
    assert_eq!(a.reserve(&s, &req, &trusted, 12)?, Dispatch::Reserved);
    drop(a);
    let mut a = Archive::open(&path)?;
    assert_eq!(
        a.reserve(&s, &req, &trusted, 13)?,
        Dispatch::AlreadyRecorded {
            state: "execution-unknown".into(),
            witness: None
        }
    );
    let witness = put(
        &a,
        f,
        Kind::Occurrence,
        "sample",
        "lab",
        json!({"execution":"observed","attempt":"1"}),
        vec![link("decision", req.decision)],
    )?;
    let signed = fixture::sign(&a, f, witness, "lab", "readback", 17)?;
    let s = a.snapshot("station", [s.id()?].into())?;
    a.reconcile_attempt(&s, "1", witness, signed, &trusted)?;
    assert!(matches!(
        a.reserve(&s, &req, &trusted, 13)?,
        Dispatch::AlreadyRecorded {
            witness: Some(_),
            ..
        }
    ));
    req.decision = second;
    req.attempt = "2".into();
    assert_eq!(
        a.reserve(&s, &req, &trusted, 13).unwrap_err().to_string(),
        "authority quota exceeded"
    );
    Ok(())
}

fn payload_check_request(
    archive: &Archive,
    f: Format,
    descriptor: Id,
    attempt: &str,
) -> Result<Request> {
    let question = put(
        archive,
        f,
        Kind::Question,
        "sample",
        "session",
        json!({"purpose":"verify retained bytes"}),
        vec![],
    )?;
    let plan = put(
        archive,
        f,
        Kind::Plan,
        "sample",
        "session",
        json!({"planned_count":1,"stopping":"one-payload","predicate":"exact-bytes"}),
        vec![link("descriptor", descriptor), link("question", question)],
    )?;
    let auth = put(
        archive,
        f,
        Kind::Authority,
        "sample",
        "session",
        json!({"actor":"analyst","operation":"verify-payload","valid":[0,100],"ceilings":{"bytes":16}}),
        vec![],
    )?;
    let sig = fixture::sign(archive, f, auth, "session", "executor", 8)?;
    let d = put(
        archive,
        f,
        Kind::Decision,
        "sample",
        "session",
        json!({"attempt":attempt,"operation":"verify-payload"}),
        vec![link("authority", auth), link("plan", plan)],
    )?;
    Ok(Request {
        attempt: attempt.into(),
        actor: "analyst".into(),
        subject: "sample".into(),
        operation: "verify-payload".into(),
        decision: d,
        authority: auth,
        signature: sig,
        costs: [("bytes".into(), 8)].into(),
    })
}

#[test]
fn capture_rolls_back_when_a_chunk_has_been_withdrawn() -> Result<()> {
    let dir = tempdir()?;
    let db = dir.path().join("archive.sqlite");
    let mut archive = Archive::open(&db)?;
    let public = Id::blob(PUBLIC);
    let retired = archive.blob(b"ret", public)?;
    let original =
        archive.selected_snapshot("original chunk", BTreeSet::new(), [retired].into())?;
    let bundle = archive.export(&original, "read")?;
    archive.withdraw(retired)?;
    let before = archive.snapshot("before capture", BTreeSet::new())?;

    let err = archive
        .capture(&b"newret"[..], "text/plain", 3, public, Format::A)
        .unwrap_err();
    assert!(err.to_string().contains("locally withdrawn chunk"), "{err}");
    assert!(err.to_string().contains(&retired.to_string()));
    archive.import(&bundle)?;
    drop(archive);

    let archive = Archive::open(&db)?;
    let after = archive.snapshot("after failed capture", BTreeSet::new())?;
    assert_eq!(after.objects, before.objects);
    assert!(!after.objects.contains(&Id::blob(b"new")));
    assert!(archive.resolve(&original, retired, "read").is_err());
    Ok(())
}

#[test]
fn retry_returns_a_not_executed_readback_after_reopening() -> Result<()> {
    let dir = tempdir()?;
    let db = dir.path().join("archive.sqlite");
    let mut a = Archive::open(&db)?;
    let f = Format::A;
    let descriptor = a.capture(&b"abcabcab"[..], "text/plain", 3, Id::blob(PUBLIC), f)?;
    let request = payload_check_request(&a, f, descriptor, "original")?;
    let follow_up = payload_check_request(&a, f, descriptor, "follow-up")?;
    assert_eq!(request.authority, follow_up.authority);
    let key = fixture::fixture_key(8);
    let keys = [hex::encode(key.verifying_key().to_bytes())].into();
    a.resource("bytes", 16)?;

    let before = a.snapshot("fixture", BTreeSet::new())?;
    assert_eq!(a.reserve(&before, &request, &keys, 50)?, Dispatch::Reserved);
    let witness = put(
        &a,
        f,
        Kind::Occurrence,
        "sample",
        "session",
        json!({"attempt":"original","execution":"not-executed"}),
        vec![link("decision", request.decision)],
    )?;
    let signature = fixture::sign(&a, f, witness, "session", "readback", 8)?;
    let head = a.snapshot("readback", [before.id()?].into())?;
    a.reconcile_attempt(&head, &request.attempt, witness, signature, &keys)?;
    drop(a);

    let mut a = Archive::open(&db)?;
    for now in [51, 52] {
        let retry = a.execute_payload_check(&before, &request, &keys, &key, now, f)?;
        assert_eq!(retry.state, "not-executed");
        assert_eq!(retry.occurrence, Some(witness));
        assert!(retry.closure.is_none());
    }
    assert_eq!(
        a.snapshot("after retry", BTreeSet::new())?.objects,
        head.objects
    );
    assert_eq!(a.reserve(&head, &follow_up, &keys, 53)?, Dispatch::Reserved);
    Ok(())
}

#[test]
fn capture_compute_close_and_retry_without_redispatch() -> Result<()> {
    let mut a = Archive::memory()?;
    let f = Format::B;
    let public = Id::blob(PUBLIC);
    let bytes = b"abcabcab";
    let descriptor = a.capture(&bytes[..], "text/plain", 3, public, f)?;
    let req = payload_check_request(&a, f, descriptor, "capture-1")?;
    let key = fixture::fixture_key(8);
    let keys = [hex::encode(key.verifying_key().to_bytes())].into();
    a.resource("bytes", 8)?;
    let s = a.snapshot("fixture", BTreeSet::new())?;
    let mut output = vec![];
    a.materialize(&s, descriptor, "read", 8, &mut output)?;
    assert_eq!(output, bytes);
    assert!(
        a.materialize(&s, descriptor, "read", 7, std::io::sink())
            .is_err()
    );
    let mut altered = a.read(&s, descriptor, "read")?;
    altered.inputs.swap(0, 1);
    let invalid = a.document(&altered, f, public)?;
    assert!(
        a.materialize(
            &a.snapshot("altered", BTreeSet::new())?,
            invalid,
            "read",
            8,
            std::io::sink()
        )
        .is_err()
    );
    let first = a.execute_payload_check(&s, &req, &keys, &key, 50, f)?;
    assert_eq!(first.state, "observed");
    let closed = first.closure.unwrap();
    let again = a.execute_payload_check(&s, &req, &keys, &key, 51, f)?;
    assert_eq!(again.closure, Some(closed));
    let head = a.snapshot("complete", [s.id()?].into())?;
    assert_eq!(
        a.read(&head, closed, "read")?.field::<String>("result")?,
        "success"
    );
    Ok(())
}

#[test]
fn finite_observation_value_uses_posterior_best_decisions() -> Result<()> {
    use naome_protocol::investigation::ObservationModel;
    let m = ObservationModel {
        losses: vec![vec![0, 10], vec![10, 0]],
        joint_masses: vec![vec![1, 0], vec![0, 1]],
        loss_unit: "points".into(),
        acquisition_cost: 1,
    };
    let value = m.evaluate()?;
    assert_eq!(
        (
            value.prior_loss_numerator,
            value.posterior_loss_numerator,
            value.net_gain_numerator,
            value.denominator
        ),
        (10, 0, 8, 2)
    );
    let uninformative = ObservationModel {
        joint_masses: vec![vec![1, 1]],
        ..m
    };
    assert_eq!(uninformative.evaluate()?.net_gain_numerator, -2);
    Ok(())
}

#[test]
fn planner_keeps_infeasible_and_uninformative_alternatives() -> Result<()> {
    use naome_protocol::investigation::Selection;
    let a = Archive::memory()?;
    let f = Format::A;
    let q = put(
        &a,
        f,
        Kind::Question,
        "reference",
        "one event",
        json!({"losses":[[0,10],[10,0]],"prior_masses":[1,1]}),
        vec![],
    )?;
    let plan = |cost: u64, joint: Value| {
        put(
            &a,
            f,
            Kind::Plan,
            "reference",
            "one event",
            json!({"action_type":"observation","objective":"decision loss","costs":{"samples":cost},"observation_model":{"losses":[[0,10],[10,0]],"joint_masses":joint,"loss_unit":"points","acquisition_cost":1}}),
            vec![link("question", q)],
        )
    };
    let chosen = plan(1, json!([[1, 0], [0, 1]]))?;
    let other = plan(1, json!([[1, 1]]))?;
    let costly = plan(3, json!([[1, 0], [0, 1]]))?;
    let s = a.snapshot("selection", BTreeSet::new())?;
    let selection = a.select(
        &s,
        &Selection {
            question: q,
            proposals: [chosen, other, costly].into(),
            ceilings: [("samples".into(), 1)].into(),
            objective: "decision loss".into(),
            loss_unit: "points".into(),
        },
        f,
    )?;
    let d = a.read(&a.snapshot("result", [s.id()?].into())?, selection, "read")?;
    assert_eq!(d.one("selected")?, chosen);
    assert_eq!(d.roles("proposal").count(), 3);
    Ok(())
}
