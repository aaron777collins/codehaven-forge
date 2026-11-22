//! Test stand-in for `codehaven hook <stage>`: the same client, without the server binary.
//! The shared hook scripts exec this program as `ch-hook hook pre-receive`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stage = match args.as_slice() {
        [h, stage] if h == "hook" => stage.as_str(),
        [stage] => stage.as_str(),
        _ => {
            eprintln!("usage: ch-hook hook <pre-receive|post-receive>");
            std::process::exit(2);
        }
    };
    std::process::exit(ch_git::hooks::client::run(stage));
}
