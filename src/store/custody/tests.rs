use super::*;
use crate::profile::{Kind, Link};
use crate::wire::Format;
use serde_json::json;

fn public() -> Id {
    Id::blob(PUBLIC)
}
fn specimen(n: u64, kind: Kind, input: Option<(Id, &str)>) -> Item {
    let links = input
        .into_iter()
        .map(|(target, role)| Link {
            target,
            role: role.into(),
            qualifier: String::new(),
        })
        .collect();
    let record = Document::new(
        kind,
        "specimen",
        "local-scale-qualification",
        json!({"ordinal":n}),
        links,
    )
    .unwrap()
    .encode(public())
    .unwrap();
    let f = Format::A;
    let bytes = record.encode(f);
    Item {
        member: Member {
            id: Id::record(f, &bytes),
            access: public(),
            position: None,
            absent: false,
        },
        bytes,
    }
}
fn store() -> Result<Archive> {
    let a = Archive::memory()?;
    Ok(a)
}
fn view(heads: &[Id]) -> Federation {
    Federation::new(heads.iter().copied().collect()).unwrap()
}

#[test]
fn exact_coverage_corrections_and_historical_receipts() -> Result<()> {
    let a = store()?;
    let raw = Item::blob(b"raw".to_vec(), public());
    let source = raw.member.id;
    let reading = specimen(0, Kind::Occurrence, Some((source, "source-input")));
    let read = reading.member.id;
    let conclusion = specimen(1, Kind::Claim, Some((read, "evidence")));
    let claim = conclusion.member.id;
    let control = specimen(2, Kind::Claim, None);
    let unrelated = control.member.id;
    let base = a
        .custody_append("lab-A", None, &[reading, conclusion, control])?
        .head;
    assert_eq!(
        view(&[base]).review(&[(&a, base)], read, 100)?.standing,
        "open"
    );
    let closed = a.custody_append("lab-A", Some(base), &[raw])?.head;
    let deps = view(&[closed]).review(&[(&a, closed)], source, 100)?;
    assert_eq!(deps.dependents, BTreeSet::from([read, claim]));
    assert_eq!(deps.standing, "review");
    let review = |head, target, budget| view(&[head]).corrections(&[(&a, head)], target, budget);
    let initial = review(closed, claim, 100)?;
    assert_eq!(initial.standing, "outside");
    let saved = a.custody_save_receipt(&initial)?;
    let notice = specimen(3, Kind::Notice, Some((source, "source-version")));
    let notice_id = notice.member.id;
    let updated = a.custody_append("lab-A", Some(closed), &[notice])?.head;
    let current = review(updated, claim, 100)?;
    assert_eq!(current.notices, BTreeSet::from([notice_id]));
    assert_eq!(current.standing, "review");
    assert_eq!(review(updated, unrelated, 100)?.standing, "outside");
    assert_eq!(review(updated, claim, 1)?.standing, "open");
    assert_eq!(
        review(closed, claim, 100)?.receipt()?,
        a.custody_receipt(saved)?
    );
    assert_eq!(
        view(&[closed, updated])
            .review(&[(&a, updated)], claim, 100)?
            .standing,
        "open"
    );
    a.withdraw(source)?;
    assert_eq!(review(updated, claim, 100)?.standing, "open");
    assert!(a.custody_resolve(closed, source, "read").is_err());
    assert_eq!(a.custody_receipt(saved)?, initial.receipt()?);
    Ok(())
}

#[test]
fn receiver_admits_verified_batches_preserves_forks_and_rechecks_grants() -> Result<()> {
    let a = store()?;
    let b = store()?;
    let lab = "origin";
    let seed = Item::blob(vec![0; BLOCK], public());
    let raw = seed.member.id;
    let first = a.custody_append(lab, None, &[seed])?.head;
    let child = |n| {
        a.custody_append(
            lab,
            Some(first),
            &[specimen(n, Kind::Claim, Some((raw, "evidence")))],
        )
    };
    let left = child(1)?.head;
    let right = child(2)?.head;
    assert_eq!(
        a.custody_tips(lab, None, 10)?
            .into_iter()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([left, right])
    );
    assert!(b.custody_pull(&a, left, 1).is_err());
    a.custody_share(left, true)?;
    a.custody_share(right, true)?;
    assert_eq!(b.custody_pull(&a, left, 1)?.cursor, first);
    assert!(b.custody_head(left).is_err());
    a.custody_share(left, false)?;
    assert!(b.custody_pull(&a, left, 1).is_err());
    a.custody_share(left, true)?;
    assert!(b.custody_pull(&a, left, 1)?.complete);
    assert_eq!(b.custody_pull(&a, left, 1)?.payload_bytes, 0);
    assert!(b.custody_pull(&a, right, 1)?.complete);
    assert_eq!(
        view(&[left, right])
            .review(&[(&b, left), (&b, right)], raw, 100)?
            .dependents
            .len(),
        2
    );
    a.withdraw(raw)?;
    assert!(b.custody_pull(&a, right, 1).is_err());
    // The independent replica retains its own custody state.
    assert_eq!(b.custody_resolve(right, raw, "read")?.len(), BLOCK);
    Ok(())
}

#[test]
fn raw_ranges_restrictions_and_failed_admissions() -> Result<()> {
    let a = store()?;
    let stream = Id::blob(b"scope");
    let mut blocks = Vec::new();
    for (block, data) in [vec![4; BLOCK], vec![9; 17]].into_iter().enumerate() {
        let mut item = Item::blob(data, Id::blob(READ_ONLY));
        item.member.position = Some(Position {
            stream,
            block: block as u64,
        });
        blocks.push(item);
    }
    let id = blocks[0].member.id;
    blocks.push(Item::blob(b"scope".to_vec(), public()));
    let head = a.custody_append("a", None, &blocks)?.head;
    assert_eq!(
        a.custody_range(head, stream, BLOCK as u64 - 2, 5)?,
        [4, 4, 9, 9, 9]
    );
    assert!(a.custody_range(head, stream, BLOCK as u64, 18).is_err());
    assert!(a.custody_resolve(head, id, "train").is_err());
    let mut invalid = Item::blob(b"complete".to_vec(), public());
    invalid.bytes.pop();
    assert!(
        a.custody_append(
            "a",
            Some(head),
            &[Item::blob(b"prefix".to_vec(), public()), invalid]
        )
        .is_err()
    );
    assert!(a.raw(Id::blob(b"prefix"))?.is_none());
    let hidden = Item::blob(br#"{"purposes":[],"version":1}"#.to_vec(), public());
    let denied = Item::blob(b"private".to_vec(), hidden.member.id);
    let restricted = a.custody_append("a", Some(head), &[hidden, denied])?.head;
    assert!(a.custody_share(restricted, true).is_err());
    assert_eq!(
        view(&[restricted])
            .review(&[(&a, restricted)], id, 50)?
            .standing,
        "open"
    );
    let root = a.custody_head(head)?.tree;
    a.db.execute("DELETE FROM custody_pages WHERE id=?", [root.to_string()])?;
    assert_eq!(
        view(&[head]).review(&[(&a, head)], id, 50)?.standing,
        "open"
    );
    Ok(())
}

// The published driver runs each phase in a new process to measure whole-process RSS.
#[test]
#[ignore = "run with tools/scale/qualify.py"]
fn qualification_sample() -> Result<()> {
    use std::{env, fs, path::PathBuf, time::Instant};
    let directory = PathBuf::from(env::var("NAOME_SAMPLE_DIR")?);
    let n: usize = env::var("NAOME_SAMPLE_SIZE")?.parse()?;
    let phase = env::var("NAOME_SAMPLE_PHASE")?;
    let path = directory.join("lab.sqlite");
    let a = Archive::open(&path)?;
    let started = Instant::now();
    let before = fs::metadata(&path)?.len();
    let tip_file = directory.join("tip");
    let result = match phase.as_str() {
        "mixed" => mixed_bursts(&directory)?,
        "inventory" => {
            let tx = a.db.unchecked_transaction()?;
            let mut tree = Tree::new(&a.db);
            let mut root = None;
            for chunk in (0..n).step_by(256) {
                let changes = (chunk..(chunk + 256).min(n))
                    .map(|i| {
                        let mut digest = [0_u8; 32];
                        digest[24..].copy_from_slice(&(i as u64).to_be_bytes());
                        let id = Id::from_blob_digest(digest);
                        (object(id), id)
                    })
                    .collect();
                root = Some(tree.apply(root, changes)?);
            }
            let root = root.unwrap();
            let initial_io = tree.io;
            let key = Id::from_blob_digest([255; 32]);
            let mut check = Tree::new(&a.db);
            assert!(check.get(root, &object(key))?.is_none());
            let height = check.io.reads;
            let mut delta = Tree::new(&a.db);
            let next = delta.apply(Some(root), BTreeMap::from([(object(key), key)]))?;
            assert!(delta.io.reads <= height && delta.io.writes <= 2 * height + 1);
            assert_eq!(check.get(next, &object(key))?, Some(key));
            assert_eq!(check.get(root, &object(key))?, None);
            tx.commit()?;
            json!({"height":height,"initial_io":initial_io,"delta_io":delta.io,"root":next})
        }
        "first" => {
            let mut parent = None;
            let mut io = PageIo::default();
            let mut payload = 0;
            for start in (0..n).step_by(MEMBERS) {
                let items: Vec<_> = (start..(start + MEMBERS).min(n))
                    .map(|i| specimen(i as u64, Kind::Occurrence, None))
                    .collect();
                payload += items.iter().map(|v| v.bytes.len()).sum::<usize>();
                let receipt = a.custody_append("independent-lab", parent, &items)?;
                io += receipt.io;
                parent = Some(receipt.head);
            }
            let head = parent.unwrap();
            a.custody_share(head, true)?;
            fs::write(tip_file, head.to_string())?;
            json!({"head":head,"payload_bytes":payload,"io":io})
        }
        "pull-first" | "pull-delta" => {
            let head: Id = fs::read_to_string(tip_file)?.parse()?;
            let dest = directory.join("receiver.sqlite");
            let b = Archive::open(&dest)?;
            let start_size = fs::metadata(&dest)?.len();
            let mut send = PageIo::default();
            let mut receive = PageIo::default();
            let mut bytes = 0;
            let mut desc = 0;
            let mut batches = 0;
            loop {
                let next = b.custody_pull(&a, head, 16)?;
                send += next.sender_io;
                receive += next.receiver_io;
                bytes += next.payload_bytes;
                desc += next.descriptor_bytes;
                batches += next.batches;
                if next.complete {
                    break;
                }
            }
            let duplicate = b.custody_pull(&a, head, 1)?;
            assert_eq!(duplicate.payload_bytes + duplicate.batches, 0);
            json!({"batches":batches,"sender_io":send,"receiver_io":receive,"payload_bytes":bytes,
                   "descriptor_bytes":desc,"receiver_growth":fs::metadata(dest)?.len()-start_size})
        }
        "delta" => {
            let base: Id = fs::read_to_string(&tip_file)?.parse()?;
            let first = specimen(0, Kind::Occurrence, None);
            let next = specimen(u64::MAX, Kind::Claim, Some((first.member.id, "evidence")));
            let target = next.member.id;
            let bytes = next.bytes.clone();
            let receipt = a.custody_append("independent-lab", Some(base), &[next])?;
            assert!(a.custody_resolve(base, target, "read").is_err());
            let (data, read_io) = a.custody_resolve_counted(receipt.head, target, "read")?;
            assert_eq!(data, bytes);
            let report =
                view(&[receipt.head]).review(&[(&a, receipt.head)], first.member.id, 100)?;
            assert_eq!(report.dependents, BTreeSet::from([target]));
            a.custody_share(receipt.head, true)?;
            fs::write(tip_file, receipt.head.to_string())?;
            json!({"payload_bytes":bytes.len(),"admit":receipt,"member_io":read_io,"reverse_io":report.io,
                   "receipt_bytes":report.receipt()?.len()})
        }
        _ => anyhow::bail!("unknown qualification phase"),
    };
    println!(
        "SAMPLE {}",
        json!({"phase":phase,"size":n,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
        "database_growth":fs::metadata(path)?.len()-before,"result":result})
    );
    Ok(())
}

#[test]
fn federation_closes_a_remote_premise_only_when_its_source_is_present() -> Result<()> {
    let a = store()?;
    let b = store()?;
    let raw = Item::blob(b"independent calibration".to_vec(), public());
    let target = raw.member.id;
    let claim = specimen(9, Kind::Claim, Some((target, "evidence")));
    let result = claim.member.id;
    let first = a.custody_append("measurements", None, &[raw])?.head;
    let second = b.custody_append("analyses", None, &[claim])?.head;
    assert_eq!(
        view(&[second])
            .review(&[(&b, second)], result, 100)?
            .standing,
        "open"
    );
    let selected = view(&[first, second]);
    let sources = [(&a, first), (&b, second)];
    assert_eq!(
        selected.review(&sources, target, 100)?.dependents,
        BTreeSet::from([result])
    );
    assert_eq!(
        selected.corrections(&sources, result, 100)?.standing,
        "outside"
    );
    Ok(())
}

fn mixed_bursts(directory: &std::path::Path) -> Result<serde_json::Value> {
    use std::{
        sync::{
            Arc, Barrier,
            mpsc::{TrySendError, sync_channel},
        },
        thread,
        time::{Duration, Instant},
    };
    let start = Instant::now();
    let (send, receive) = sync_channel::<(usize, Id)>(2);
    let barrier = Arc::new(Barrier::new(2));
    let mut writers = Vec::new();
    for lab in 0..2 {
        let signal = send.clone();
        let dir = directory.to_owned();
        let ready = barrier.clone();
        writers.push(thread::spawn(move || -> Result<(u64, usize)> {
            let a = Archive::open(dir.join(format!("lab-{lab}.sqlite")))?;
            let source = format!("lab-{lab}");
            let mut tip = None;
            let mut stalled = 0;
            let stream = Id::blob(source.as_bytes());
            ready.wait();
            for seq in 0..12_u64 {
                let mut data = vec![lab as u8; if seq % 4 == 3 { BLOCK } else { 1024 }];
                data[..8].copy_from_slice(&seq.to_be_bytes());
                let mut raw = Item::blob(data, public());
                raw.member.position = Some(Position { stream, block: seq });
                let record = specimen(seq, Kind::Occurrence, Some((raw.member.id, "evidence")));
                let mut members = vec![raw, record];
                if tip.is_none() {
                    members.push(Item::blob(source.as_bytes().to_vec(), public()));
                }
                let head = a.custody_append(&source, tip, &members)?.head;
                tip = Some(head);
                if seq % 3 == 2 {
                    a.custody_share(head, true)?;
                    match signal.try_send((lab, head)) {
                        Ok(()) => {}
                        Err(TrySendError::Full(msg)) => {
                            stalled += 1;
                            signal.send(msg)?;
                        }
                        Err(err) => anyhow::bail!("{err}"),
                    }
                }
            }
            Ok((start.elapsed().as_micros() as u64, stalled))
        }));
    }
    drop(send);
    let mut received = 0;
    let mut queue_full = 0;
    let mut writer_us = Vec::new();
    let mut payload = 0;
    let mut desc = 0;
    let mut duplicates = 0;
    let mut largest = 0;
    let mut tips = BTreeMap::new();
    for (lab, head) in receive {
        let origin = Archive::open(directory.join(format!("lab-{lab}.sqlite")))?;
        let path = directory.join(format!("replica-{lab}.sqlite"));
        loop {
            let db = Archive::open(&path)?;
            let next = db.custody_pull(&origin, head, 1)?;
            received += next.batches;
            payload += next.payload_bytes;
            desc += next.descriptor_bytes;
            largest = largest.max(next.payload_bytes);
            // Close the receiver at each service boundary, including incomplete transfers.
            drop(db);
            thread::sleep(Duration::from_millis(3));
            if next.complete {
                break;
            }
        }
        let db = Archive::open(&path)?;
        let duplicate = db.custody_pull(&origin, head, 1)?;
        assert_eq!(duplicate.payload_bytes + duplicate.batches, 0);
        duplicates += 1;
        tips.insert(lab, head);
    }
    for join in writers {
        let (time, stalled) = join.join().expect("writer panicked")?;
        writer_us.push(time);
        queue_full += stalled;
    }
    assert_eq!((received, duplicates, tips.len()), (24, 8, 2));
    let a = Archive::open(directory.join("replica-0.sqlite"))?;
    let b = Archive::open(directory.join("replica-1.sqlite"))?;
    let heads = [tips[&0], tips[&1]];
    let merged = view(&heads);
    let raw = a.custody_head(heads[0])?.delta[0].id;
    let report = merged.review(&[(&a, heads[0]), (&b, heads[1])], raw, 100)?;
    // Every raw block also contributes to its stream. The measurement links to this particular block.
    assert_eq!(report.dependents.len(), 2);
    let last = a.custody_range(heads[0], Id::blob(b"lab-0"), 11 * BLOCK as u64, 8)?;
    assert_eq!(last, 11_u64.to_be_bytes());
    Ok(
        json!({"writers":2,"batches":received,"announcements":8,"queue_capacity":2,
        "queue_full_events":queue_full,"writer_finish_us":writer_us,"service_pause_ms":3,
        "receiver_reopens":received,"duplicate_deliveries":duplicates,"max_batch_payload":largest,
        "payload_bytes":payload,"descriptor_bytes":desc,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0,
        "federated_tips":heads,"review_standing":report.standing}),
    )
}

#[test]
fn portable_frames_verify_pinned_ancestry_and_rollback_partial_input() -> Result<()> {
    let a = store()?;
    let b = store()?;
    let mut tips = Vec::new();
    let mut parent = None;
    for n in 0..9 {
        let h = a
            .custody_append(
                "instrument-A",
                parent,
                &[specimen(n, Kind::Occurrence, None)],
            )?
            .head;
        parent = Some(h);
        tips.push(h);
    }
    let tip = tips[8];
    a.custody_share(tip, true)?;
    let mut after = None;
    for next in tips {
        let mut frame = Vec::new();
        a.custody_export_step(tip, after, &mut frame)?;
        assert!(
            b.custody_import_step(tip, &frame[..frame.len() - 1])
                .is_err()
        );
        assert!(b.custody_head(next).is_err());
        assert!(
            b.custody_import_step(Id::blob(b"a different pin"), frame.as_slice())
                .is_err()
        );
        let result = b.custody_import_step(tip, frame.as_slice())?;
        assert_eq!(next, result.head);
        assert_eq!(b.custody_import_step(tip, frame.as_slice())?.head, next);
        after = Some(next);
    }
    Ok(())
}

#[test]
fn a_pinned_but_incomplete_index_cannot_exclude_a_notice() -> Result<()> {
    let a = store()?;
    let b = store()?;
    let original = Item::blob(b"source version".to_vec(), public());
    let target = original.member.id;
    let base = a.custody_append("lab", None, &[original])?.head;
    a.custody_share(base, true)?;
    b.custody_pull(&a, base, 1)?;
    let notice = specimen(7, Kind::Notice, Some((target, "source-version")));
    let bytes = notice.bytes.clone();
    let notice_id = notice.member.id;
    let tip = a.custody_append("lab", Some(base), &[notice])?.head;
    let mut claimed = a.custody_head(tip)?;
    claimed.tree = a.custody_head(base)?.tree;
    let metadata = serde_json::to_vec(&claimed)?;
    let pin = claimed.manifest()?.id()?;
    let mut frame = super::frame::MAGIC.to_vec();
    for field in [&metadata, &bytes] {
        frame.extend_from_slice(&(field.len() as u32).to_be_bytes());
        frame.extend_from_slice(field);
    }
    assert_eq!(
        b.custody_import_step(pin, frame.as_slice())
            .err()
            .unwrap()
            .to_string(),
        "derived inventory or coverage differs"
    );
    assert!(b.custody_head(pin).is_err());
    assert!(b.custody_raw(notice_id).is_err());
    assert_eq!(b.custody_tips("lab", None, 10)?, vec![base]);
    let unscoped = a
        .custody_append("lab", Some(base), &[specimen(8, Kind::Notice, None)])?
        .head;
    assert_eq!(
        view(&[unscoped])
            .corrections(&[(&a, unscoped)], target, 100)?
            .standing,
        "open"
    );
    Ok(())
}

#[test]
fn physical_page_splices_preserve_roots_and_bound_backing_reads() -> Result<()> {
    let a = store()?;
    let mut tree = Tree::new(&a.db);
    let keys: Vec<_> = (0_u64..100)
        .map(|i| {
            let mut digest = [0; 32];
            digest[24..].copy_from_slice(&i.to_be_bytes());
            let id = Id::from_blob_digest(digest);
            (object(id), id)
        })
        .collect();
    let base = tree.apply(None, keys.into_iter().collect())?;
    let key = Id::from_blob_digest([255; 32]);
    let next = tree.apply(Some(base), BTreeMap::from([(object(key), key)]))?;
    assert_eq!(
        next.to_string(),
        "1:1:099a070fe4dbf60cec1a0cce433c011d4a8455bd11bfd605c1f2d282f6eeaa50"
    );
    let mut head = next;
    for i in 0..20 {
        let id = Id::blob(&[i]);
        head = tree.apply(Some(head), BTreeMap::from([(object(id), id)]))?;
        assert_eq!(tree.get(head, &object(id))?, Some(id));
        assert!(tree.get(base, &object(id))?.is_none());
    }
    assert!(tree.io.backing_reads > tree.io.reads && tree.io.backing_reads <= 5 * tree.io.reads);
    assert!(tree.io.backing_written_bytes < tree.io.written_bytes);
    Ok(())
}
