mod empty_blob;
mod excludes;
mod gitcmp;
mod ipc;
mod objects;
mod snapshot;
mod spawn;
mod stats;
mod sweep;
mod synth;
mod util;
mod walk;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("hookprobe") => spawn::hookprobe(rest),
        Some("objects") => objects::run(rest),
        Some("sweep") => sweep::run(rest),
        Some("snapshot") => snapshot::run(rest),
        Some("spawn") => spawn::run(),
        Some("empty-blob") => empty_blob::run(),
        Some("excludes") => excludes::run(rest),
        Some("synth") => synth::run(rest),
        _ => Err(
            "usage: baton-bench <objects|sweep|snapshot|spawn|empty-blob|excludes|synth> ..."
                .into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
