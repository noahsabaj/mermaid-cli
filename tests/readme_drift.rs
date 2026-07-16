//! Drift guards for README.md — the same registry-backed truth pattern the
//! prompt tests use (`advertised_slash_commands_exist`). The README documents
//! slash commands and ships a sample config; both rot silently without these.

use mermaid_cli::domain::slash_commands::COMMAND_REGISTRY;

const README: &str = include_str!("../README.md");

/// Backticked `/command` tokens. Path-like tokens (`/dev/tty`) are skipped —
/// the name is followed by another `/` — and adjacent-backtick constructs
/// like `` `a`/`b` `` produce empty names, which are skipped too.
fn backticked_commands(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("`/") {
        let tail = &rest[pos + 2..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if !name.is_empty() && !tail[name.len()..].starts_with('/') {
            out.push(name);
        }
        rest = tail;
    }
    out
}

/// Tokens the scanner reads as commands but that are deliberately NOT in the
/// registry. Every entry needs a reason; a real built-in never belongs here.
const NOT_BUILTINS: &[&str] = &[
    // Backticked absolute filesystem paths (`` `/dev` ``).
    "dev", "tmp", "etc", "proc",
    // Example of a PLUGIN-defined prompt command in the plugins section —
    // dynamic by design, so it can't be in the static registry.
    "deploy",
];

#[test]
fn readme_slash_commands_exist() {
    let commands = backticked_commands(README);
    assert!(
        !commands.is_empty(),
        "expected the README to document slash commands"
    );
    for name in commands {
        if NOT_BUILTINS.contains(&name.as_str()) {
            continue;
        }
        assert!(
            COMMAND_REGISTRY
                .iter()
                .any(|c| c.name == name || c.aliases.contains(&name.as_str())),
            "README documents `/{name}` but no such slash command is registered"
        );
    }
}

#[test]
fn readme_sample_config_parses() {
    // The fenced toml block containing [safety] is the sample config users
    // copy; it must always parse as a real Config. Config does not deny
    // unknown fields, so this catches syntax and type drift rather than
    // renames — the explicit anchors below cover the load-bearing names.
    let block = README
        .split("```toml")
        .skip(1)
        .map(|rest| rest.split("```").next().unwrap_or(""))
        .find(|block| block.contains("[safety]"))
        .expect("README must contain the sample config with a [safety] section");
    let config: mermaid_cli::app::Config =
        toml::from_str(block).expect("README sample config must parse as a valid Config");
    assert!(
        matches!(config.safety.mode, mermaid_cli::runtime::SafetyMode::Ask),
        "sample config's documented default must stay ask"
    );
    assert!(
        block.contains("external_writes"),
        "the BREAKING external-writes knob must stay documented"
    );
    assert!(
        block.contains("system_installs"),
        "the BREAKING system-installs knob must stay documented"
    );
}
