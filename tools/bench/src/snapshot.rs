use crate::objects::signature;
use crate::stats;
use crate::util::{Res, arg_path, bench_root, fresh_dir, git, git_ok};
use crate::walk;
use gix::objs::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::PreviousValue;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TURNS: usize = 200;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

struct Baseline {
    tree: gix::ObjectId,
    commit: gix::ObjectId,
    /// path, size, parent directory entry count
    candidates: Vec<(String, u64, usize)>,
}

#[derive(Clone, Copy)]
enum Editor {
    /// `Repository::edit_tree`, which probes the object store once per entry of every tree it writes.
    HighLevel,
    /// `gix::objs::tree::Editor` with a write closure and no probes.
    Plumbing,
}

#[derive(Default)]
struct Stages {
    blobs: Vec<Duration>,
    tree: Vec<Duration>,
    commit: Vec<Duration>,
    reference: Vec<Duration>,
    total: Vec<Duration>,
}

fn build_baseline(
    repo: &gix::Repository,
    root: &std::path::Path,
    files: &[walk::Stat],
) -> Res<Baseline> {
    let mut parent_counts: HashMap<&str, usize> = HashMap::new();
    for f in files {
        *parent_counts
            .entry(f.path.rsplit_once('/').map_or("", |(d, _)| d))
            .or_default() += 1;
    }

    let t_total = Instant::now();
    let mut entries = Vec::with_capacity(files.len());
    let (mut read_time, mut write_time) = (Duration::ZERO, Duration::ZERO);
    let (mut bytes, mut skipped, mut links) = (0u64, 0usize, 0usize);
    for f in files {
        let full = root.join(&f.path);
        let t = Instant::now();
        let md = std::fs::symlink_metadata(&full)?;
        let (kind, data) = if md.file_type().is_symlink() {
            links += 1;
            (
                EntryKind::Link,
                std::fs::read_link(&full)?
                    .to_string_lossy()
                    .replace('\\', "/")
                    .into_bytes(),
            )
        } else if md.len() > MAX_FILE_BYTES {
            skipped += 1;
            continue;
        } else {
            (EntryKind::Blob, std::fs::read(&full)?)
        };
        read_time += t.elapsed();
        let t = Instant::now();
        let id = repo.write_blob(&data)?.detach();
        write_time += t.elapsed();
        bytes += data.len() as u64;
        entries.push((f.path.as_str(), kind, id));
    }

    let t_tree = Instant::now();
    let mut editor = repo.edit_tree(gix::ObjectId::empty_tree(repo.object_hash()))?;
    for (path, kind, id) in &entries {
        editor.upsert(*path, *kind, *id)?;
    }
    let tree = editor.write()?.detach();
    let tree_time = t_tree.elapsed();
    let commit = write_commit(repo, "baseline\n", tree, &[])?;
    println!(
        "baseline: {} files ({:.1} MiB, {links} symlinks, {skipped} over 64 MiB skipped) in {:.0} ms",
        entries.len(),
        bytes as f64 / 1_048_576.0,
        t_total.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "  read files {:.0} ms, write blobs {:.0} ms, edit and write trees {:.0} ms",
        read_time.as_secs_f64() * 1000.0,
        write_time.as_secs_f64() * 1000.0,
        tree_time.as_secs_f64() * 1000.0
    );
    let candidates = files
        .iter()
        .filter(|f| (1024..=256 * 1024).contains(&f.size))
        .map(|f| {
            let dir = f.path.rsplit_once('/').map_or("", |(d, _)| d);
            (f.path.clone(), f.size, parent_counts[dir])
        })
        .collect();
    Ok(Baseline {
        tree,
        commit,
        candidates,
    })
}

fn pick(candidates: &[(String, u64, usize)], bigdir: bool) -> Res<Vec<&(String, u64, usize)>> {
    if candidates.len() < 3 {
        return Err("fewer than 3 files between 1 KiB and 256 KiB".into());
    }
    if bigdir {
        let mut by_dir: HashMap<&str, Vec<&(String, u64, usize)>> = HashMap::new();
        for c in candidates {
            by_dir
                .entry(c.0.rsplit_once('/').map_or("", |(d, _)| d))
                .or_default()
                .push(c);
        }
        let best = by_dir
            .into_values()
            .filter(|v| v.len() >= 3)
            .max_by_key(|v| (v[0].2, v[0].0.clone()))
            .ok_or("no directory holds 3 candidate files")?;
        Ok(best.into_iter().take(3).collect())
    } else {
        let n = candidates.len();
        Ok(vec![
            &candidates[n / 4],
            &candidates[n / 2],
            &candidates[3 * n / 4],
        ])
    }
}

fn turns(
    repo: &gix::Repository,
    root: &std::path::Path,
    base: &Baseline,
    files: &[&(String, u64, usize)],
    editor: Editor,
    ref_name: &str,
) -> Res<(Stages, gix::ObjectId)> {
    let originals: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(root.join(&f.0)))
        .collect::<Result<_, _>>()?;
    let mut prev_commit = base.commit;
    let mut prev_tree = base.tree;
    repo.reference(
        ref_name,
        prev_commit,
        PreviousValue::MustNotExist,
        "baseline",
    )?;
    let mut s = Stages::default();
    for i in 0..TURNS {
        let t0 = Instant::now();
        let ids: Vec<gix::ObjectId> = originals
            .iter()
            .map(|orig| {
                let mut data = orig.clone();
                data.extend_from_slice(format!("\n// {ref_name} turn {i}\n").as_bytes());
                repo.write_blob(&data).map(|id| id.detach())
            })
            .collect::<Result<_, _>>()?;
        let t1 = Instant::now();

        let tree = match editor {
            Editor::HighLevel => {
                let mut e = repo.edit_tree(prev_tree)?;
                for (f, id) in files.iter().zip(&ids) {
                    e.upsert(f.0.as_str(), EntryKind::Blob, *id)?;
                }
                e.write()?.detach()
            }
            Editor::Plumbing => {
                let root_tree: gix::objs::Tree = repo.find_tree(prev_tree)?.decode()?.into();
                let mut e =
                    gix::objs::tree::Editor::new(root_tree, &repo.objects, repo.object_hash());
                for (f, id) in files.iter().zip(&ids) {
                    e.upsert(f.0.as_str().split('/'), EntryKind::Blob, *id)?;
                }
                e.write(|t| repo.write_object(t).map(|id| id.detach()))?
            }
        };
        let t2 = Instant::now();

        let message =
            format!("turn {i}\n\nBaton-Thread: bench\nBaton-Turn: {i}\nBaton-Agent: bench\n");
        let commit = write_commit(repo, &message, tree, &[prev_commit])?;
        let t3 = Instant::now();

        repo.reference(
            ref_name,
            commit,
            PreviousValue::MustExistAndMatch(Target::Object(prev_commit)),
            "baton turn",
        )?;
        let t4 = Instant::now();

        s.blobs.push(t1 - t0);
        s.tree.push(t2 - t1);
        s.commit.push(t3 - t2);
        s.reference.push(t4 - t3);
        s.total.push(t4 - t0);
        prev_commit = commit;
        prev_tree = tree;
    }
    Ok((s, prev_commit))
}

pub fn run(args: &[String]) -> Res<()> {
    let root = arg_path(args, "worktree")?;
    let user_repo = gix::open(&root)?;
    let sweep = walk::gix_dirwalk(&user_repo, &root, true)?;
    let name = root
        .file_name()
        .map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
    let side_dir = fresh_dir(&bench_root(), &format!("snapshot-{name}.git"))?;
    let side = gix::init_bare(&side_dir)?;

    println!("snapshot of {}", root.display());
    let base = build_baseline(&side, &root, &sweep.files)?;

    let series = [
        (
            "refs/baton/bench",
            false,
            Editor::HighLevel,
            "edit_tree, 3 files spread",
        ),
        (
            "refs/baton/bench-bigdir",
            true,
            Editor::HighLevel,
            "edit_tree, 3 files in biggest dir",
        ),
        (
            "refs/baton/bench-plumbing",
            false,
            Editor::Plumbing,
            "plumbing editor, spread",
        ),
        (
            "refs/baton/bench-plumbing-bigdir",
            true,
            Editor::Plumbing,
            "plumbing editor, biggest dir",
        ),
    ];
    let mut results = Vec::new();
    for (ref_name, bigdir, editor, label) in series {
        let files = pick(&base.candidates, bigdir)?;
        let paths: Vec<_> = files.iter().map(|f| f.0.as_str()).collect();
        let (stages, tip) = turns(&side, &root, &base, &files, editor, ref_name)?;
        let depth = files
            .iter()
            .map(|f| f.0.matches('/').count())
            .max()
            .unwrap_or(0);
        let siblings = files.iter().map(|f| f.2).max().unwrap_or(0);
        results.push((
            label,
            ref_name,
            stages,
            tip,
            paths.join(", "),
            depth,
            siblings,
        ));
    }

    stats::header();
    for (label, _, stages, _, _, _, _) in &mut results {
        stats::row(&format!("{label}: total"), &mut stages.total);
        for (stage, v) in [
            ("blobs", &mut stages.blobs),
            ("tree", &mut stages.tree),
            ("commit", &mut stages.commit),
            ("ref CAS", &mut stages.reference),
        ] {
            stats::row(&format!("    {stage}"), v);
        }
    }
    for (label, _, _, _, paths, depth, siblings) in &results {
        println!(
            "{label}: files {paths} (max depth {depth}, up to {siblings} entries in their directory)"
        );
    }

    for (_, ref_name, _, tip, ..) in &results {
        let head = git_ok(&side_dir, &["rev-parse", ref_name])?;
        let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        let count = git_ok(&side_dir, &["rev-list", "--count", ref_name])?;
        println!(
            "check {ref_name}: ref {} tip, {} commits (expected {})",
            if gix::ObjectId::from_hex(head.as_bytes()).is_ok_and(|h| h == *tip) {
                "is the"
            } else {
                "IS NOT the"
            },
            String::from_utf8_lossy(&count.stdout).trim(),
            TURNS + 1
        );
    }
    let fsck = git(&side_dir)
        .args(["fsck", "--connectivity-only", "--no-dangling"])
        .output()?;
    println!(
        "check: git fsck --connectivity-only exit {}",
        fsck.status.code().unwrap_or(-1)
    );
    Ok(())
}

fn write_commit(
    repo: &gix::Repository,
    message: &str,
    tree: gix::ObjectId,
    parents: &[gix::ObjectId],
) -> Res<gix::ObjectId> {
    let sig = signature();
    let (mut author_buf, mut committer_buf) = (
        gix::date::parse::TimeBuf::default(),
        gix::date::parse::TimeBuf::default(),
    );
    let commit = repo.new_commit_as(
        sig.to_ref(&mut committer_buf),
        sig.to_ref(&mut author_buf),
        message,
        tree,
        parents.iter().copied(),
    )?;
    Ok(commit.id)
}
