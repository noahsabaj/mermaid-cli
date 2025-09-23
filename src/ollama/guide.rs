/// Detect OS and provide Ollama installation instructions
pub fn detect_and_guide() {
    println!("[WARNING] Ollama not found on your system\n");

    #[cfg(target_os = "macos")]
    {
        println!("To use local models, install Ollama:");
        println!("[INSTALL] macOS: brew install ollama");
        println!("   or");
        println!("[DOWNLOAD] Download: https://ollama.com/download/mac\n");
    }

    #[cfg(target_os = "linux")]
    {
        println!("To use local models, install Ollama:");
        println!("[INSTALL] Linux: curl -fsSL https://ollama.com/install.sh | sh");
        println!("   or");
        println!("[DOWNLOAD] Download: https://ollama.com/download/linux\n");
    }

    #[cfg(target_os = "windows")]
    {
        println!("To use local models, install Ollama:");
        println!("[DOWNLOAD] Windows: Download from https://ollama.com/download/windows\n");
    }

    println!("After installing Ollama:");
    println!("1. Start Ollama: ollama serve");
    println!("2. Run mermaid again!");
    println!("\nAlternatively, use cloud models with --model openai/gpt-4o");
}