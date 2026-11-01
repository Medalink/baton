use crate::stats;
use crate::util::{Res, Rng, arg_path, fresh_dir, git};
use gix::objs::{Kind, Write as _, WriteTo as _};
use std::sync::{Arc, mpsc};
use std::time::Instant;

const ITERATIONS: usize = 200;
const THREADS: usize = 4;

pub struct Obj {
    pub kind: Kind,
    pub data: Vec<u8>,
    pub id: gix::ObjectId,
}

fn obj(kind: Kind, data: Vec<u8>) -> Res<Obj> {
    let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, &data)?;
    Ok(Obj { kind, data, id })
}

pub fn signature() -> gix::actor::Signature {
    gix::actor::Signature {
        name: "baton".into(),
        email: "baton@localhost".into(),
        time: gix::date::Time::new(1_761_900_000, 0),
    }
}

/// Three blobs of 2, 7 and 20 KiB, eight chained trees and one commit, all unique per iteration
/// so no write is skipped as already present.
pub fn turn_objects(rng: &mut Rng, iteration: usize) -> Res<Vec<Obj>> {
    let mut out = Vec::with_capacity(12);
    for (i, size) in [2048usize, 7168, 20480].into_iter().enumerate() {
        let mut data = format!("iteration {iteration} blob {i}\n").into_bytes();
        data.extend(rng.text(size));
        out.push(obj(Kind::Blob, data)?);
    }
    let blob_ids: Vec<_> = out.iter().map(|o| o.id).collect();
    let mut previous: Option<gix::ObjectId> = None;
    for t in 0..8 {
        let mut entries: Vec<gix::objs::tree::Entry> = blob_ids
            .iter()
            .enumerate()
            .map(|(b, id)| gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: format!("f{t}_{b}.rs").into(),
                oid: *id,
            })
            .collect();
        if let Some(id) = previous {
            entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: "sub".into(),
                oid: id,
            });
        }
        let mut data = Vec::new();
        gix::objs::Tree { entries }.write_to(&mut data)?;
        let tree = obj(Kind::Tree, data)?;
        previous = Some(tree.id);
        out.push(tree);
    }
    let commit = gix::objs::Commit {
        tree: previous.expect("eight trees"),
        parents: Default::default(),
        author: signature(),
        committer: signature(),
        encoding: None,
        message: format!("turn {iteration}\n\nBaton-Thread: bench\n").into(),
        extra_headers: Vec::new(),
    };
    let mut data = Vec::new();
    commit.write_to(&mut data)?;
    out.push(obj(Kind::Commit, data)?);
    Ok(out)
}

fn write_all(repo: &gix::Repository, objs: &[Obj]) -> Res<()> {
    for o in objs {
        let id = repo.objects.write_buf(o.kind, &o.data)?;
        assert_eq!(id, o.id, "object written under a different id");
    }
    Ok(())
}

fn write_pack(repo: &gix::Repository, objs: &[Obj]) -> Res<()> {
    use gix_pack::data::output;
    let entries = objs
        .iter()
        .map(|o| {
            let count = output::Count::from_data(o.id, None);
            let data = gix::objs::Data::new(&o.data, o.kind, gix::hash::Kind::Sha1);
            output::Entry::from_data(&count, &data, gix_zlib::Compression::DEFAULT)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut bytes = Vec::new();
    let input = std::iter::once(Ok::<_, gix::Error>(entries));
    let writer = output::bytes::FromEntriesIter::new(
        input,
        &mut bytes,
        objs.len() as u32,
        gix_pack::data::Version::V2,
        gix::hash::Kind::Sha1,
    );
    for chunk in writer {
        chunk?;
    }
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let pack_dir = repo.objects.store_ref().path().join("pack");
    gix_pack::Bundle::write_to_directory(
        &mut bytes.as_slice(),
        Some(&pack_dir),
        &mut gix::progress::Discard,
        &interrupt,
        None::<gix::objs::find::Never>,
        gix::hash::Kind::Sha1,
        gix_pack::bundle::write::Options {
            thread_limit: Some(1),
            ..Default::default()
        },
    )?;
    Ok(())
}

type Job = (Arc<Vec<Obj>>, usize);

fn spawn_workers(repo: &gix::Repository) -> (Vec<mpsc::Sender<Job>>, mpsc::Receiver<Res<()>>) {
    let (done_tx, done_rx) = mpsc::channel();
    let senders = (0..THREADS)
        .map(|_| {
            let (tx, rx) = mpsc::channel::<Job>();
            let repo = repo.clone();
            let done = done_tx.clone();
            std::thread::spawn(move || {
                for (objs, worker) in rx {
                    let res = (|| -> Res<()> {
                        for o in objs.iter().skip(worker).step_by(THREADS) {
                            repo.objects.write_buf(o.kind, &o.data)?;
                        }
                        Ok(())
                    })();
                    let _ = done.send(res);
                }
            });
            tx
        })
        .collect();
    (senders, done_rx)
}

pub fn run(args: &[String]) -> Res<()> {
    let dir = arg_path(args, "dir")?;
    std::fs::create_dir_all(&dir)?;
    let mut serial = Vec::new();
    let mut parallel = Vec::new();
    let mut pack = Vec::new();

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let side = fresh_dir(&dir, "objects-serial.git")?;
    let repo = gix::init_bare(&side)?;
    for i in 0..ITERATIONS {
        let objs = turn_objects(&mut rng, i)?;
        let t = Instant::now();
        write_all(&repo, &objs)?;
        serial.push(t.elapsed());
    }
    let serial_check = verify(&side, "serial", ITERATIONS * 12)?;

    let mut premade = Vec::new();
    let mut parallel_checks = Vec::new();
    for prefan in [false, true] {
        let side = fresh_dir(&dir, "objects-parallel.git")?;
        let repo = gix::init_bare(&side)?;
        if prefan {
            for b in 0..=255u8 {
                std::fs::create_dir_all(side.join("objects").join(format!("{b:02x}")))?;
            }
        }
        let (senders, done) = spawn_workers(&repo);
        let samples = if prefan { &mut premade } else { &mut parallel };
        for i in 0..ITERATIONS {
            let objs = Arc::new(turn_objects(&mut rng, 1000 + i)?);
            let t = Instant::now();
            for (worker, tx) in senders.iter().enumerate() {
                tx.send((objs.clone(), worker))?;
            }
            for _ in 0..THREADS {
                done.recv()??;
            }
            samples.push(t.elapsed());
        }
        let label = if prefan {
            "parallel, fan-out dirs pre-created"
        } else {
            "parallel"
        };
        parallel_checks.push(verify(&side, label, ITERATIONS * 12)?);
    }

    let side = fresh_dir(&dir, "objects-pack.git")?;
    let repo = gix::init_bare(&side)?;
    for i in 0..ITERATIONS {
        let objs = turn_objects(&mut rng, 2000 + i)?;
        let t = Instant::now();
        write_pack(&repo, &objs)?;
        pack.push(t.elapsed());
    }
    let pack_check = verify(&side, "pack", ITERATIONS * 12)?;

    println!("objects: 12 per turn (3 blobs 2/7/20 KiB, 8 trees, 1 commit), {ITERATIONS} turns");
    let slowest = parallel
        .iter()
        .enumerate()
        .max_by_key(|(_, d)| **d)
        .map(|(i, _)| i)
        .unwrap_or(0);
    println!("slowest 4-thread turn: #{slowest}");
    let firsts = [serial[0], parallel[0], pack[0]];
    stats::header();
    stats::row("loose, serial", &mut serial);
    stats::row("loose, 4 threads", &mut parallel);
    stats::row("loose, 4 threads, dirs pre-made", &mut premade);
    stats::row("one pack per turn (gix-pack)", &mut pack);
    println!(
        "first turn, before any warm-up: serial {:.3} ms, 4 threads {:.3} ms, pack {:.3} ms",
        firsts[0].as_secs_f64() * 1000.0,
        firsts[1].as_secs_f64() * 1000.0,
        firsts[2].as_secs_f64() * 1000.0
    );
    println!("verification (git cat-file --batch-all-objects, git fsck):");
    for line in std::iter::once(serial_check)
        .chain(parallel_checks)
        .chain([pack_check])
    {
        println!("  {line}");
    }
    Ok(())
}

/// Counts the objects git itself can enumerate, so the timed work is proven to be on disk.
fn verify(side: &std::path::Path, label: &str, expected: usize) -> Res<String> {
    let out = git(side)
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype)",
        ])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let n = out
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .count();
    let fsck = git(side).args(["fsck", "--no-dangling"]).output()?;
    Ok(format!(
        "{label}: {n} objects visible to git (expected {expected}), fsck exit {}",
        fsck.status.code().unwrap_or(-1)
    ))
}
