//! What happens to a turn after it is committed: build `baton.turn.v1`, classify it, store the verdict,
//! and in live mode tell the notifiers. None of it is on the hook's path or the turn's. A pool of
//! threads takes the work from a bounded queue, so a slow classifier delays only other verdicts, and a
//! full queue drops the verdict with a log line instead of growing without limit.
//!
//! Every failure here fails open: the turn is already recorded, so the worst result is a missing verdict.

use super::log::Log;
use super::writer::Writer;
use baton_core::{
    Classifier, Dirs, Lens, LensAnswer, Mode, NotifyDoc, Plugins, ProjectKey, Result, SideRepo,
    StorePolicy, TurnDocSource, TurnRecord, TurnVerdict, open_policy, run_command, run_for_effect,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

const WORKERS: usize = 4;
const QUEUE: usize = 64;
/// A lens is read into every classifier call, so it is kept small.
const LENS_MAX_BYTES: u64 = 64 * 1024;

pub struct Work {
    pub root: PathBuf,
    pub project: ProjectKey,
    pub record: TurnRecord,
    pub writer: Writer,
}

#[derive(Clone)]
pub struct Runner {
    tx: SyncSender<Work>,
    log: Arc<Log>,
}

impl Runner {
    pub fn start(dirs: Dirs, log: Arc<Log>) -> Self {
        let (tx, rx) = sync_channel::<Work>(QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        for n in 0..WORKERS {
            let (rx, dirs, worker_log) = (Arc::clone(&rx), dirs.clone(), Arc::clone(&log));
            let spawned = std::thread::Builder::new()
                .name(format!("baton-plugins-{n}"))
                .spawn(move || work(&rx, &dirs, &worker_log));
            if let Err(e) = spawned {
                log.line(&format!("could not start a plug-in worker: {e}"));
            }
        }
        Self { tx, log }
    }

    /// Queues the turn and returns at once.
    pub fn submit(&self, work: Work) {
        let id = work.record.id.short();
        match self.tx.try_send(work) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self
                .log
                .line(&format!("plug-ins are busy; no verdict for {id}")),
            Err(TrySendError::Disconnected(_)) => self
                .log
                .line(&format!("plug-in workers stopped; no verdict for {id}")),
        }
    }
}

fn work(rx: &Mutex<Receiver<Work>>, dirs: &Dirs, log: &Log) {
    loop {
        let next = rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        let Ok(job) = next else { return };
        let id = job.record.id.short();
        // A panic in one job must not take a worker with it.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process(&job, dirs, log)));
        match outcome {
            Ok(Ok(Some(done))) => log.line(&format!("verdict {id} {done}")),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => log.line(&format!("no verdict for {id}: {e}")),
            Err(_) => log.line(&format!("no verdict for {id}: the plug-in worker panicked")),
        }
    }
}

/// The verdict line for the log, or `None` when classification is off.
fn process(job: &Work, dirs: &Dirs, log: &Log) -> Result<Option<String>> {
    let plugins = Plugins::load(&dirs.user_config())?;
    if plugins.mode == Mode::Off {
        return Ok(None);
    }
    let policy = open_policy(&job.root, dirs)?;
    let side = SideRepo::open_existing(&dirs.side_repo(&job.project))?;
    let lens = read_lens(&plugins, job, &policy, log);
    let turn = TurnDocSource {
        side: &side,
        record: &job.record,
        project: &job.project,
        root: &job.root,
        policy: &policy,
    }
    .build(lens)?;

    let (verdict, source) = match &plugins.classifier {
        Classifier::Rules(rules) => (rules.classify(&turn), "rules"),
        Classifier::Command(spec) => {
            let verdict: TurnVerdict = run_command(spec, &turn)?;
            (
                verdict.redacted(&|text| policy.redact_text(text)),
                "command",
            )
        }
    };
    job.writer
        .store_verdict(job.record.id, verdict.clone(), source)?;
    let mut line = format!(
        "class={} findings={} source={source}",
        verdict.class,
        verdict.findings.len()
    );

    if plugins.mode == Mode::Live {
        let doc = NotifyDoc {
            verdict: &verdict,
            turn: &turn,
        };
        for notify in plugins
            .notify
            .iter()
            .filter(|n| n.on.contains(&verdict.class))
        {
            if let Err(e) = run_for_effect(&notify.command, &doc) {
                log.line(&format!(
                    "notifier {} failed for {}: {e}",
                    notify.command.command[0],
                    job.record.id.short()
                ));
            }
        }
    } else {
        line.push_str(" mode=shadow");
    }
    Ok(Some(line))
}

#[derive(Serialize)]
struct LensAsk<'a> {
    schema: &'static str,
    project: String,
    thread: &'a str,
    cwd: &'a Path,
}

/// The lens for this turn, or `None`: a missing file or a failing command costs the classifier its lens,
/// not the turn its verdict. What a lens holds is redacted, since it goes to commands and into notices.
fn read_lens(plugins: &Plugins, job: &Work, policy: &StorePolicy, log: &Log) -> Option<String> {
    let lens = plugins.lens.as_ref()?;
    let text = match lens {
        Lens::File(relative) => read_inside(&job.root, relative),
        Lens::Command(spec) => {
            let ask = LensAsk {
                schema: "baton.lens.v1",
                project: job.project.to_string(),
                thread: job.record.turn.draft.thread.as_str(),
                cwd: &job.root,
            };
            run_command::<_, LensAnswer>(spec, &ask)
                .map(|answer| answer.lens)
                .map_err(|e| e.to_string())
        }
    };
    match text {
        Ok(text) => {
            let text: String = text.chars().take(LENS_MAX_BYTES as usize).collect();
            Some(policy.redact_text(&text))
        }
        Err(e) => {
            log.line(&format!("the lens could not be read: {e}"));
            None
        }
    }
}

/// A clone can carry a symlink, so a lens file that resolves outside the repository is not read.
fn read_inside(root: &Path, relative: &Path) -> std::result::Result<String, String> {
    let path = root.join(relative);
    let real = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    if !real.starts_with(&root) {
        return Err(format!(
            "{} resolves outside the repository",
            path.display()
        ));
    }
    read_capped(&real)
}

fn read_capped(path: &Path) -> std::result::Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(LENS_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
