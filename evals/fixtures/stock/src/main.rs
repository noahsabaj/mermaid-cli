use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: stock <export.csv>");
        return ExitCode::from(2);
    };
    let loaded = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| stock::load::load(&text));
    match loaded {
        Ok(items) => {
            print!("{}", stock::summary::render(&items));
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("{path}: {why}");
            ExitCode::FAILURE
        }
    }
}
