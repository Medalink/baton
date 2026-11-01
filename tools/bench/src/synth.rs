use crate::util::{Res, Rng, arg_path, fresh_dir, git_ok};
use std::fs;
use std::path::Path;

fn put(root: &Path, rel: &str, body: &[u8]) -> Res<()> {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("file has a parent"))?;
    fs::write(path, body)?;
    Ok(())
}

/// A small repo with every ignore feature the design relies on: nested `.gitignore`, negation,
/// anchoring, directory-only rules, `.git/info/exclude`, a repo-local `core.excludesFile`, a
/// tracked file that matches an ignore rule, an empty file, and a nested repository.
pub fn run(args: &[String]) -> Res<()> {
    let root = fresh_dir(&arg_path(args, "dir")?, "synthetic")?;
    git_ok(&root, &["init", "-q"])?;
    git_ok(&root, &["config", "user.name", "bench"])?;
    git_ok(&root, &["config", "user.email", "bench@localhost"])?;
    let excludes_file = root
        .parent()
        .expect("synthetic repo has a parent")
        .join("synthetic-global-excludes");
    fs::write(&excludes_file, "*.global-ignored\n")?;
    git_ok(
        &root,
        &[
            "config",
            "core.excludesFile",
            &excludes_file.to_string_lossy().replace('\\', "/"),
        ],
    )?;

    let mut rng = Rng(42);
    put(
        &root,
        ".gitignore",
        b"/build/\n*.log\n!keep.log\nnode_modules/\n/secret.txt\n",
    )?;
    put(
        &root,
        "src/.gitignore",
        b"generated/\n*.tmp\n!important.tmp\n",
    )?;
    put(
        &root,
        "src/deep/nested/.gitignore",
        b"*\n!.gitignore\n!kept.rs\n",
    )?;
    for d in 0..40 {
        for f in 0..25 {
            put(
                &root,
                &format!("src/mod{d}/file{f}.rs"),
                &rng.text(1500 + f * 40),
            )?;
        }
    }
    put(
        &root,
        "src/generated/out.rs",
        b"ignored by src/.gitignore\n",
    )?;
    put(
        &root,
        "src/mod1/generated/out.rs",
        b"ignored by src/.gitignore\n",
    )?;
    put(&root, "src/a.tmp", b"ignored\n")?;
    put(&root, "src/important.tmp", b"negated back in\n")?;
    put(
        &root,
        "src/deep/nested/.gitignore",
        b"*\n!.gitignore\n!kept.rs\n",
    )?;
    put(&root, "src/deep/nested/kept.rs", b"fn kept() {}\n")?;
    put(&root, "src/deep/nested/dropped.rs", b"fn dropped() {}\n")?;
    put(&root, "build/out.bin", b"ignored dir\n")?;
    put(
        &root,
        "src/build/not-anchored.rs",
        b"/build/ is anchored, so this is kept\n",
    )?;
    put(&root, "debug.log", b"ignored\n")?;
    put(&root, "keep.log", b"negated back in\n")?;
    put(&root, "node_modules/pkg/index.js", b"ignored dir\n")?;
    put(&root, "secret.txt", b"anchored ignore\n")?;
    put(
        &root,
        "docs/secret.txt",
        b"anchored rule does not reach here\n",
    )?;
    put(
        &root,
        "excluded-locally.txt",
        b"matches .git/info/exclude\n",
    )?;
    put(
        &root,
        "notes.global-ignored",
        b"matches core.excludesFile\n",
    )?;
    put(&root, "empty.txt", b"")?;
    put(&root, "README.md", b"# synthetic\n")?;
    put(
        &root,
        "tracked-but-ignored.log",
        b"tracked, then matched by *.log\n",
    )?;
    fs::write(root.join(".git/info/exclude"), "excluded-locally.txt\n")?;

    git_ok(&root, &["add", "-f", "-A"])?;
    git_ok(
        &root,
        &[
            "rm",
            "-q",
            "--cached",
            "debug.log",
            "build/out.bin",
            "node_modules/pkg/index.js",
            "secret.txt",
        ],
    )?;
    git_ok(
        &root,
        &[
            "rm",
            "-q",
            "--cached",
            "-r",
            "src/generated",
            "src/mod1/generated",
        ],
    )?;
    git_ok(
        &root,
        &[
            "rm",
            "-q",
            "--cached",
            "src/a.tmp",
            "src/deep/nested/dropped.rs",
            "excluded-locally.txt",
            "notes.global-ignored",
        ],
    )?;
    git_ok(&root, &["commit", "-q", "-m", "synthetic"])?;

    let nested = root.join("vendor").join("inner");
    fs::create_dir_all(&nested)?;
    git_ok(&nested, &["init", "-q"])?;
    put(&nested, "inner.txt", b"file in a nested repository\n")?;
    put(&root, "untracked-new.rs", b"fn new() {}\n")?;
    let count = fs::read_dir(root.join("src"))?.count();
    println!(
        "synthetic repo at {} ({count} entries under src/)",
        root.display()
    );
    Ok(())
}
