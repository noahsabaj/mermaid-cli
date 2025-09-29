// Integration tests for proxy configuration

use mermaid::proxy::{get_compose_dir, is_container_runtime_available};
use std::env;

#[test]
fn test_container_runtime_detection() {
    let runtime = is_container_runtime_available();
    // This test will pass if either podman or docker is installed
    // In CI, we should have at least one
    if runtime.is_none() {
        eprintln!("Warning: No container runtime found (podman or docker)");
    }
}

#[test]
fn test_compose_dir_resolution() {
    // Should not panic and should return a valid path
    let result = get_compose_dir();
    assert!(
        result.is_ok(),
        "get_compose_dir should resolve to a valid path"
    );

    let path = result.unwrap();
    assert!(
        path.exists() || path.parent().map(|p| p.exists()).unwrap_or(false),
        "Compose dir or its parent should exist: {}",
        path.display()
    );
}

#[test]
fn test_custom_proxy_dir() {
    // Test MERMAID_PROXY_DIR environment variable
    let temp_dir = env::temp_dir().join("mermaid_test_proxy");
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Create a test docker-compose.yml
    std::fs::write(temp_dir.join("docker-compose.yml"), "version: '3.9'").unwrap();

    env::set_var("MERMAID_PROXY_DIR", temp_dir.to_str().unwrap());

    let result = get_compose_dir();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), temp_dir);

    // Cleanup
    env::remove_var("MERMAID_PROXY_DIR");
    std::fs::remove_dir_all(&temp_dir).ok();
}
