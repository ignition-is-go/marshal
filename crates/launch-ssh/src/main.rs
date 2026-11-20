//! `marshal-launch-ssh <recover|check|launch|inspect|stop|status>`: JSON request on stdin,
//! JSON response on stdout, reasons on stderr. See the crate docs.

use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut verb = args.next().unwrap_or_default();
    if verb == "--target-worker" {
        if let Err(error) = marshal_launch_ssh::target_worker(&args.collect::<Vec<_>>()) {
            eprintln!("marshal-launch-ssh target worker: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    let target = verb == "--target";
    if target {
        verb = args.next().unwrap_or_default();
    }
    match verb.as_str() {
        "-V" | "--version" => {
            println!("marshal-launch-ssh {}", marshal_launch_ssh::VERSION);
            return;
        }
        "recover" | "check" | "launch" | "inspect" | "stop" | "status" => {}
        _ => {
            eprintln!(
                "marshal-launch-ssh: unknown verb `{verb}`\n\
                 usage: marshal-launch-ssh <recover|check|launch|inspect|stop|status>  (one JSON request on stdin)\n\
                 A marshal-launch adapter; see docs/agent-launcher.md."
            );
            std::process::exit(2);
        }
    }
    let mut input = String::new();
    let result = std::io::stdin()
        .read_to_string(&mut input)
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            if target {
                marshal_launch_ssh::run_target(&verb, &input)
            } else {
                marshal_launch_ssh::run(&verb, &input)
            }
        });
    match result {
        Ok(v) if target => println!("@@marshal-target {v}"),
        Ok(v) => println!("{v}"),
        Err(e) => {
            eprintln!("marshal-launch-ssh {verb}: {e:#}");
            std::process::exit(1);
        }
    }
}
