mod endpoint;
mod hook;

fn main() {
    // Nothing runs before this: the hook must not pay for argument parsing, config, logging or gix.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|first| first == "hook") {
        hook::run(args.next().and_then(|a| a.into_string().ok()));
    }
    eprintln!("baton: only `hook` exists so far");
    std::process::exit(2);
}
