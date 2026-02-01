/// Constants module to avoid magic numbers in the codebase
// Network Configuration
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;
pub const DEFAULT_SEARXNG_PORT: u16 = 8888;

// Timeouts
pub const COMMAND_TIMEOUT_SECS: u64 = 30;
pub const HTTP_REQUEST_TIMEOUT_SECS: u64 = 600; // 10 minutes for large model requests

// UI Configuration
pub const UI_REFRESH_INTERVAL_MS: u64 = 50;
pub const UI_SCROLL_LINES: u16 = 3;
pub const UI_DEFAULT_VIEWPORT_HEIGHT: u16 = 20;
pub const UI_STATUS_MESSAGE_THRESHOLD: u16 = 3; // For auto-scroll detection
pub const UI_ERROR_LOG_MAX_SIZE: usize = 50; // Maximum number of errors to keep in log

// Model Token Limits
pub const GPT4_32K_CONTEXT: usize = 32768;
pub const GPT4_TURBO_CONTEXT: usize = 128000;
pub const GPT35_CONTEXT: usize = 16384;
pub const CLAUDE_3_OPUS_CONTEXT: usize = 200000;
pub const CLAUDE_3_SONNET_CONTEXT: usize = 200000;
pub const CLAUDE_3_HAIKU_CONTEXT: usize = 200000;
pub const CLAUDE_25_CONTEXT: usize = 100000;
pub const OLLAMA_DEFAULT_CONTEXT: usize = 32768;
pub const GROQ_LLAMA_CONTEXT: usize = 32768;
pub const GROQ_MIXTRAL_CONTEXT: usize = 32768;
pub const GROQ_DEFAULT_CONTEXT: usize = 8192;
pub const GEMINI_15_PRO_CONTEXT: usize = 1048576; // 1M tokens

// Default Model Configuration
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_MAX_TOKENS: usize = 4096;
pub const DEFAULT_TOP_P: f32 = 1.0;

// File Patterns
pub const DEFAULT_EXCLUDE_PATTERNS: &[&str] = &[
    "*.log",
    "*.tmp",
    ".git/*",
    ".env",
    "target/*",
    "node_modules/*",
    "__pycache__/*",
    ".venv/*",
    "venv/*",
    "*.pyc",
    "*.pyo",
    ".DS_Store",
    "Thumbs.db",
    "*.swp",
    "*.swo",
    "*~",
    ".idea/*",
    ".vscode/*",
    "*.iml",
    ".pytest_cache/*",
    ".mypy_cache/*",
    ".ruff_cache/*",
    "dist/*",
    "build/*",
    "*.egg-info/*",
];

// Dangerous Commands (for safety checks)
pub const DANGEROUS_COMMANDS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -rf ~",
    "rm -rf ~/",
    "format c:",
    "del /f /s /q c:",
    ":(){ :|:& };:", // Fork bomb
    "mkfs",
    "dd if=/dev/zero",
    "chmod -R 777 /",
    "chmod -R 000 /",
    "chown -R",
    "> /dev/sda",
    "wget -O - | sh",
    "curl -s | bash",
];

// SSH Key Patterns (for security)
pub const SSH_KEY_FILES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".pem",
    ".key",
    ".pfx",
];
