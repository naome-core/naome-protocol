use anyhow::Result;
use naome_protocol::{
    profile::{self, PUBLIC, READ_ONLY},
    store::{
        Archive, Snapshot,
        sharing::{MAX_MANIFEST_PARENTS, MAX_MEMBER_BYTES, MAX_MEMBERS, Member, Replica},
    },
    wire::{Format, Id},
};

fn replica(source: &Archive, s: &Snapshot) -> Result<Replica> {
    let wire = serde_json::to_vec(s)?;
    let mut replica = Replica::new(s.id()?, serde_json::from_slice(&wire)?)?;
    for packet in source.export(s, "read")?.packets {
        replica.accept(
            packet.id,
            Member {
                id: packet.id,
                blob_access: packet.id.format().is_none().then_some(packet.access),
                bytes: hex::decode(packet.hex)?,
            },
        )?;
    }
    Ok(replica)
}

#[test]
fn only_released_readable_members_are_served_and_mirrors_preserve_rights() -> Result<()> {
    let source = Archive::memory()?;
    let dest = Archive::memory()?;
    let id = source.blob(b"read-only specimen", Id::blob(READ_ONLY))?;
    let s = source.snapshot("lab", Default::default())?;
    let head = s.id()?;
    assert!(source.released_member(head, id).is_err());
    source.release(head)?;
    let outside = source.blob(b"not released", Id::blob(PUBLIC))?;
    assert!(source.released_member(head, outside).is_err());
    assert_eq!(
        replica(&source, &source.released_manifest(head)?)?.commit(&dest)?,
        head
    );
    assert_eq!(
        dest.released_member(head, id)?.blob_access,
        Some(Id::blob(READ_ONLY))
    );
    assert!(dest.resolve(&s, id, "train").is_err());
    dest.withdraw(id)?;
    assert!(!dest.releases()?.contains(&head));
    assert!(dest.observed_releases()?.contains(&head));
    assert_eq!(dest.head(head)?, s);
    assert!(dest.released_member(head, id).is_err());
    assert!(replica(&source, &s)?.commit(&dest).is_err());
    assert!(dest.resolve(&s, id, "read").is_err());

    let denied = source.blob(br#"{"purposes":[],"version":1}"#, Id::blob(PUBLIC))?;
    let unrestricted_head = source.snapshot("lab with another descriptor", [head].into())?;
    let mut direct = Replica::new(unrestricted_head.id()?, unrestricted_head.clone())?;
    for packet in source.export(&unrestricted_head, "read")?.packets {
        direct.accept(
            packet.id,
            Member {
                id: packet.id,
                blob_access: Some(if packet.id == id {
                    denied
                } else {
                    packet.access
                }),
                bytes: hex::decode(packet.hex)?,
            },
        )?;
    }
    let fresh = Archive::memory()?;
    assert!(direct.commit(&fresh).is_err());
    assert!(fresh.observed_releases()?.is_empty());
    assert!(fresh.resolve(&unrestricted_head, id, "read").is_err());
    source.blob(b"withheld", denied)?;
    let private = source.snapshot("restricted", [head].into())?.id()?;
    assert!(source.release(private).is_err());
    assert!(source.released_manifest(private).is_err());
    Ok(())
}

#[test]
fn selection_rejects_denied_and_unavailable_dependencies() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let private = archive.blob(br#"{"purposes":[],"version":1}"#, public)?;
    let denied = archive.blob(b"private reading", private)?;
    let erased = archive.blob(b"erased reading", public)?;
    archive.withdraw(erased)?;
    for (dependency, reason) in [
        (denied, "use withheld"),
        (erased, "unavailable"),
        (Id::blob(b"never deposited"), "unavailable"),
    ] {
        let record = profile::Document::new(
            profile::Kind::Occurrence,
            "sample",
            "run",
            serde_json::json!({}),
            vec![profile::link("source", dependency)],
        )?;
        let root = archive.document(&record, Format::A, public)?;
        let err = archive
            .selected_snapshot("selected", Default::default(), [root].into())
            .unwrap_err();
        assert!(err.to_string().contains(reason), "{err}");
    }
    assert!(archive.releases()?.is_empty());
    Ok(())
}

#[test]
fn frames_must_match_the_requested_identity_and_inventory_before_commit() -> Result<()> {
    let source = Archive::memory()?;
    let dest = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let id = source.blob(b"measurement", public)?;
    let s = source.snapshot("lab", Default::default())?;
    let head = s.id()?;
    source.release(head)?;
    assert!(Replica::new(public, s.clone()).is_err());
    let mut partial = Replica::new(head, s.clone())?;
    assert!(
        partial
            .accept(public, source.released_member(head, id)?)
            .is_err()
    );
    assert!(
        partial
            .accept(
                id,
                Member {
                    id,
                    blob_access: Some(public),
                    bytes: vec![0xff]
                }
            )
            .is_err()
    );
    assert!(
        partial
            .accept(
                id,
                Member {
                    id,
                    blob_access: Some(public),
                    bytes: vec![0; MAX_MEMBER_BYTES + 1]
                }
            )
            .is_err()
    );
    let mut bad = source.released_member(head, id)?;
    bad.blob_access = None;
    assert!(partial.accept(id, bad).is_err());
    let mut unknown = source.released_member(head, id)?;
    unknown.blob_access = Some(Id::blob(b"missing access"));
    assert!(partial.accept(id, unknown).is_err());
    partial.accept(id, source.released_member(head, id)?)?;
    assert!(
        partial
            .accept(id, source.released_member(head, id)?)
            .is_err()
    );
    assert!(partial.commit(&dest).is_err());
    assert!(dest.head(head).is_err());
    assert!(dest.resolve(&s, id, "read").is_err());
    let mut parents: std::collections::BTreeSet<_> = (0..MAX_MANIFEST_PARENTS)
        .map(|n| Id::blob(&n.to_le_bytes()))
        .collect();
    let with_parents = source.seal_members("parents", parents.clone(), s.objects.clone())?;
    Replica::new(with_parents.id()?, with_parents)?;
    parents.insert(Id::blob(b"additional parent"));
    assert!(
        source
            .seal_members("parents", parents, s.objects.clone())
            .is_err()
    );
    let members = (0..=MAX_MEMBERS)
        .map(|n| Id::blob(&n.to_le_bytes()))
        .collect();
    let oversized = source.seal_members("oversized", Default::default(), members)?;
    assert!(Replica::new(oversized.id()?, oversized).is_err());
    Ok(())
}

#[test]
fn access_conflicts_and_bad_signatures_roll_back_the_whole_release() -> Result<()> {
    let source = Archive::memory()?;
    let dest = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let id = source.blob(b"prior protected sample", public)?;
    let new = source.blob(b"another sample", public)?;
    dest.blob(b"prior protected sample", Id::blob(READ_ONLY))?;
    let s = source.snapshot("first", Default::default())?;
    assert!(replica(&source, &s)?.commit(&dest).is_err());
    assert!(dest.releases()?.is_empty());
    assert!(dest.resolve(&s, new, "read").is_err());
    assert!(dest.resolve(&s, id, "train").is_err());

    let mut attestation = profile::attest(
        id,
        "bench",
        "calibration",
        "research-policy-1",
        &ed25519_dalek::SigningKey::from_bytes(&[1; 32]),
    )?;
    attestation
        .fields
        .insert("signature".into(), "00".repeat(64).into());
    source.document(&attestation, Format::A, public)?;
    let forged = source.snapshot("bad signature", Default::default())?;
    assert!(source.release(forged.id()?).is_err());
    let clean = Archive::memory()?;
    assert!(replica(&source, &forged)?.commit(&clean).is_err());
    assert!(clean.head(forged.id()?).is_err());
    assert!(clean.resolve(&forged, new, "read").is_err());
    Ok(())
}

#[cfg(feature = "cli")]
#[test]
fn an_authentic_but_unadmitted_correction_does_not_change_the_judgment() -> Result<()> {
    use naome_protocol::{
        evaluation::{Standing, assess},
        fixture,
    };
    let source = Archive::memory()?;
    let dest = Archive::memory()?;
    let case = fixture::seed(&source, Format::A)?;
    let notice = fixture::notice(&source, Format::A, case.calibration, 80, 9)?;
    let s = source.snapshot("other laboratory", [case.published].into())?;
    replica(&source, &s)?.commit(&dest)?;
    let result = assess(&dest, &s, &case.queries[2])?;
    assert!(result.visible_notices.contains(&notice));
    assert!(!result.applicable_notices.contains(&notice));
    assert_eq!(result.result, Some(Standing::Supported));
    Ok(())
}

#[test]
fn record_access_comes_from_the_envelope() -> Result<()> {
    let source = Archive::memory()?;
    let access = Id::blob(READ_ONLY);
    let doc = profile::Document::new(
        profile::Kind::Occurrence,
        "sample",
        "bench",
        serde_json::json!({}),
        vec![],
    )?;
    let id = source.document(&doc, Format::A, access)?;
    let snapshot = source.snapshot("lab", Default::default())?;
    let head = snapshot.id()?;
    source.release(head)?;
    let mut receiver = Replica::new(head, snapshot)?;
    let mut inconsistent = source.released_member(head, id)?;
    inconsistent.blob_access = Some(Id::blob(PUBLIC));
    assert!(receiver.accept(id, inconsistent).is_err());
    let promoted = doc.encode(Id::blob(PUBLIC))?.encode(Format::A);
    assert!(
        receiver
            .accept(
                id,
                Member {
                    id,
                    blob_access: None,
                    bytes: promoted
                }
            )
            .is_err()
    );
    receiver.accept(id, source.released_member(head, id)?)?;
    Ok(())
}

#[test]
fn replicated_releases_do_not_replace_the_local_search_head() -> Result<()> {
    let source = Archive::memory()?;
    let target = Archive::memory()?;
    let work = target.snapshot("local work", Default::default())?.id()?;
    let peer = source.snapshot("peer release", Default::default())?;
    let peer_id = replica(&source, &peer)?.commit(&target)?;

    assert!(target.releases()?.contains(&peer_id));
    assert_eq!(target.search_head(None)?.id()?, work);
    assert_eq!(target.search_head(Some(peer_id))?.id()?, peer_id);
    Ok(())
}

#[test]
fn selected_records_can_be_transferred_before_their_access_descriptor() -> Result<()> {
    use naome_protocol::store::custody::MEMBERS;
    use std::collections::BTreeSet;
    let source = Archive::memory()?;
    let mut records = BTreeSet::new();
    for n in 0..MEMBERS {
        let doc = profile::Document::new(
            profile::Kind::Occurrence,
            "sample",
            "bench",
            serde_json::json!({"index": n}),
            vec![],
        )?;
        records.insert(source.document(&doc, Format::A, Id::blob(PUBLIC))?);
    }
    let selected = source.selected_snapshot("selected", Default::default(), records.clone())?;
    let target = selected.id()?;
    source.custody_share(target, true)?;
    let direct = Archive::memory()?;
    let portable = Archive::memory()?;
    let first = direct.custody_pull(&source, target, 1)?;
    assert!(!first.complete);
    assert_eq!(direct.head(first.cursor)?.objects, records);
    let mut frame = Vec::new();
    source.custody_export_step(target, None, &mut frame)?;
    let prefix = portable.custody_import_step(target, frame.as_slice())?.head;
    assert_eq!(prefix, first.cursor);
    source.custody_share(target, false)?;
    assert!(direct.custody_pull(&source, target, 1).is_err());
    assert!(
        source
            .custody_export_step(target, Some(prefix), Vec::new())
            .is_err()
    );
    source.custody_share(target, true)?;
    assert!(direct.custody_pull(&source, target, 1)?.complete);
    frame.clear();
    source.custody_export_step(target, Some(prefix), &mut frame)?;
    assert_eq!(
        portable.custody_import_step(target, frame.as_slice())?.head,
        target
    );
    assert_eq!(direct.head(target)?, selected);
    assert_eq!(portable.head(target)?, selected);
    Ok(())
}

#[test]
fn publication_and_replication_charge_the_same_linked_witnesses() -> Result<()> {
    use naome_protocol::store::sharing::MAX_RELEASE_BYTES;
    use std::collections::BTreeSet;

    let source = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    for n in 0..24 {
        source.blob(&[n], public)?;
    }
    let linked = source.snapshot("linked", Default::default())?;
    let link = source.deposit_manifest(&linked)?;
    let mut roots = BTreeSet::from([link]);
    for n in 0..15 {
        roots.insert(source.blob(&vec![n; MAX_MEMBER_BYTES], public)?);
    }
    let select = |size| -> Result<Snapshot> {
        let mut members = roots.clone();
        members.insert(source.blob(&vec![0xff; size], public)?);
        source.selected_snapshot("release", Default::default(), members)
    };
    // All main inventory keys have fixed width and fit in a single leaf.
    let sample = select(16)?;
    source.release(sample.id()?)?;
    let packet = serde_json::to_value(source.released_manifest(sample.id()?)?)?;
    let mut used = 0;
    for page in packet["pages"].as_object().unwrap().values() {
        used += serde_json::to_vec(page)?.len();
    }
    assert!(packet["pages"].as_object().unwrap().len() > 1);
    for &id in &sample.objects {
        used += source.resolve(&sample, id, "read")?.len();
    }
    let remaining = MAX_RELEASE_BYTES - used + 16;
    let exact = select(remaining)?.id()?;
    source.release(exact)?;
    let mut receiver = Replica::new(exact, source.released_manifest(exact)?)?;
    while let Some(id) = receiver.next_member() {
        receiver.accept(id, source.released_member(exact, id)?)?;
    }
    assert_eq!(receiver.commit(&Archive::memory()?)?, exact);
    let over = select(remaining + 1)?.id()?;
    assert!(source.release(over).is_err());
    assert!(!source.has_observed_release(over)?);
    Ok(())
}
