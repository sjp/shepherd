use std::process::ExitCode;

fn main() -> ExitCode {
    agentbus_cli::run(std::env::args_os())
}
