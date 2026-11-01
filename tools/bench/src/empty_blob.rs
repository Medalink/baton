use crate::util::{Res, bench_root, fresh_dir, git};
use gix::objs::{Kind, Write as _};

type Writer = fn(&gix::Repository) -> Res<gix::ObjectId>;

pub fn run() -> Res<()> {
    let cases: [(&str, Kind, Writer); 5] = [
        ("repo.write_blob(b\"\")", Kind::Blob, |r| {
            Ok(r.write_blob(b"")?.detach())
        }),
        ("repo.write_buf(Blob, b\"\")", Kind::Blob, |r| {
            Ok(r.objects.write_buf(Kind::Blob, b"")?)
        }),
        ("repo.write_object(empty Blob)", Kind::Blob, |r| {
            Ok(r.write_object(gix::objs::Blob { data: Vec::new() })?
                .detach())
        }),
        ("repo.write_object(empty Tree)", Kind::Tree, |r| {
            Ok(r.write_object(gix::objs::Tree::empty())?.detach())
        }),
        ("repo.write_buf(Tree, b\"\")", Kind::Tree, |r| {
            Ok(r.objects.write_buf(Kind::Tree, b"")?)
        }),
    ];

    println!("empty objects: does the call put the object on disk?");
    println!(
        "{:<32} {:<18} {:<16} {:<10} {:<14} {:<16} {:<10}",
        "call",
        "has_object before",
        "git -e before",
        "has_object",
        "loose file",
        "git -e after",
        "oid"
    );
    for (i, (name, kind, write)) in cases.iter().enumerate() {
        let dir = fresh_dir(&bench_root(), &format!("empty-{i}.git"))?;
        let repo = gix::init_bare(&dir)?;
        let known = gix::objs::compute_hash(gix::hash::Kind::Sha1, *kind, b"")?;
        let before_has = repo.has_object(known);
        let before_git = git(&dir)
            .args(["cat-file", "-e", &known.to_string()])
            .output()?;
        let id = write(&repo)?;
        let hex = id.to_string();
        let loose = dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        let probe = git(&dir).args(["cat-file", "-e", &hex]).output()?;
        let known = gix::objs::compute_hash(gix::hash::Kind::Sha1, *kind, b"")?;
        assert_eq!(id, known, "write returned an unexpected id");
        println!(
            "{:<32} {:<18} {:<16} {:<10} {:<14} {:<16} {}",
            name,
            before_has,
            format!("exit {}", before_git.status.code().unwrap_or(-1)),
            repo.has_object(id),
            if loose.exists() { "on disk" } else { "absent" },
            format!("exit {}", probe.status.code().unwrap_or(-1)),
            &hex[..10]
        );
    }
    Ok(())
}
