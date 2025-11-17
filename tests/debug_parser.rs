#[test]
fn debug_cargo_test_parsing() {
    use mermaid_cli::agents::parse_actions;

    let response = "[COMMAND: cargo test]\n[/COMMAND]";
    let actions = parse_actions(response);

    eprintln!("Response: {}", response);
    eprintln!("Actions parsed: {}", actions.len());
    for (i, action) in actions.iter().enumerate() {
        eprintln!("Action {}: {:?}", i, action);
    }

    assert!(actions.len() > 0, "Should parse at least one action");
}
