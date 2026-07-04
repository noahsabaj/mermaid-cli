/// Check if Ollama is installed on the system. Delegates to the same binary
/// discovery the server autostart uses (PATH first, then the platform
/// installer's default locations), so the startup preflight can never reject
/// a user the autostart would have healed — e.g. a fresh install whose PATH
/// edit hasn't reached this shell yet.
pub fn is_installed() -> bool {
    super::server::find_binary().is_some()
}
