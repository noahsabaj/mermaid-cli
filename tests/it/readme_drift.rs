//! Drift guards for the shipped markdown — the same registry-backed truth
//! pattern the prompt tests use (`advertised_slash_commands_exist`). The README
//! documents slash commands and a starter config; `docs/configuration.md`
//! carries the annotated full schema. All of it rots silently without these.

use mermaid_domain::slash_commands::COMMAND_REGISTRY;

const README: &str = include_str!("../../README.md");
const CONFIG_DOC: &str = include_str!("../../docs/configuration.md");

/// The fenced toml block holding `[safety]` — the sample users copy.
///
/// It must always parse as a real `Config`. `Config` does not deny unknown
/// fields, so this catches syntax and type drift rather than renames; the
/// explicit anchors at each call site cover the load-bearing names.
fn safety_config_block(doc: &str) -> &str {
    doc.split("```toml")
        .skip(1)
        .map(|rest| rest.split("```").next().unwrap_or(""))
        .find(|block| block.contains("[safety]"))
        .expect("document must contain a sample config with a [safety] section")
}

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
    let block = safety_config_block(README);
    let config: mermaid_domain::Config =
        toml::from_str(block).expect("README sample config must parse as a valid Config");
    assert!(
        matches!(config.safety.mode, mermaid_runtime::SafetyMode::Ask),
        "sample config's documented default must stay ask"
    );
}

#[test]
fn config_doc_schema_parses() {
    let block = safety_config_block(CONFIG_DOC);
    let config: mermaid_domain::Config =
        toml::from_str(block).expect("docs/configuration.md schema must parse as a valid Config");
    assert!(
        matches!(config.safety.mode, mermaid_runtime::SafetyMode::Ask),
        "documented default must stay ask"
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

/// Fails when the README links to a `docs/` file that is not there.
///
/// This repo shipped four dead `docs/*.md` links and they survived, because
/// nothing looked.
#[test]
fn readme_doc_links_resolve() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    // Splitting on the opening delimiter drops the `docs/` prefix, so each
    // fragment starts at the file name; anchors ride along after a `#`.
    let targets: Vec<&str> = README
        .split("](docs/")
        .skip(1)
        .filter_map(|tail| tail.split(')').next())
        .filter_map(|link| link.split('#').next())
        .collect();
    assert!(
        !targets.is_empty(),
        "expected the README to link into docs/"
    );
    for name in targets {
        assert!(
            docs.join(name).exists(),
            "README links to docs/{name}, which does not exist"
        );
    }
}
