//! `mermaidd`: the daemon binary. Everything it does lives in
//! `mermaid_cli::mermaidd`; this file parses two flags and calls [`run`].

#[cfg(any(unix, windows))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use mermaid_cli::mermaidd::cli::{CliAction, HELP, classify_args};
    match classify_args(std::env::args().skip(1)) {
        CliAction::Run => {},
        CliAction::Version => {
            println!("mermaidd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        },
        CliAction::Help => {
            print!("{HELP}");
            return Ok(());
        },
        CliAction::Unknown(arg) => {
            eprintln!("mermaidd: unrecognized argument '{arg}'\n\n{HELP}");
            std::process::exit(2);
        },
    }
    mermaid_cli::mermaidd::run().await
}

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!("mermaidd currently supports Unix and Windows only");
    std::process::exit(1);
}
