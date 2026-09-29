use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: ledger <file>");
        return ExitCode::from(2);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ledger::parse::parse_ledger(&text) {
        Ok(entries) => {
            print!("{}", ledger::report::render(&entries));
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("{path}: {why}");
            ExitCode::FAILURE
        }
    }
}
