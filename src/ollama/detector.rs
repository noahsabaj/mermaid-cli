/// Check if Ollama is installed on the system
pub fn is_installed() -> bool {
    which::which("ollama").is_ok()
}
