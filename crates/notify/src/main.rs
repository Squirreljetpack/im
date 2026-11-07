use clap::Parser;
use notify::NotifyCli;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = NotifyCli::parse();
    if let Err(err) = cli.run() {
        eprintln!("notify: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
