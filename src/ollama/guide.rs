/// Detect OS and provide Ollama installation instructions.
///
/// Prints to STDERR: this guidance fires from inside model resolution, which
/// also runs under `doctor --format json` / `feedback --format json` — where
/// stdout is a machine-readable payload that a stray warning would corrupt.
pub fn detect_and_guide() {
    eprintln!("[WARNING] Ollama not found on your system\n");

    #[cfg(target_os = "macos")]
    {
        eprintln!("To use local models, install Ollama:");
        eprintln!("[INSTALL] macOS: brew install ollama");
        eprintln!("   or");
        eprintln!("[DOWNLOAD] Download: https://ollama.com/download/mac\n");
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("To use local models, install Ollama:");
        eprintln!("[INSTALL] Linux: curl -fsSL https://ollama.com/install.sh | sh");
        eprintln!("   or");
        eprintln!("[DOWNLOAD] Download: https://ollama.com/download/linux\n");
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!("To use local models, install Ollama:");
        eprintln!("[DOWNLOAD] Windows: Download from https://ollama.com/download/windows\n");
    }

    eprintln!("After installing Ollama:");
    eprintln!("1. Start Ollama: ollama serve");
    eprintln!("2. Pull a model: ollama pull qwen3:8b");
    eprintln!("3. Run mermaid again!");
}
