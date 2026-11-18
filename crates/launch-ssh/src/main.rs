//! `marshal-launch-ssh <recover|check|launch|inspect|stop|status>`: JSON request on stdin,
//! JSON response on stdout, reasons on stderr. See the crate docs.

use std::io::Read;

fn main() {
    let verb = std::env::args().nth(1).unwrap_or_default();
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
        .and_then(|_| marshal_launch_ssh::run(&verb, &input));
    match result {
        Ok(v) => println!("{v}"),
        Err(e) => {
            eprintln!("marshal-launch-ssh {verb}: {e:#}");
            std::process::exit(1);
        }
    }
}
