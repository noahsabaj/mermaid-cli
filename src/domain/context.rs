//! Loaded project context: instructions, memory, skills.
//!
//! Value types only. `State` holds all three and the reducer injects their
//! rendered indexes into the model prompt, so they belong at or below the pure
//! core. Discovering and reading the files that produce them stays in
//! `src/app/{instructions,memory,skills}.rs`, which walk the filesystem.

use std::path::PathBuf;
use std::time::SystemTime;

/// One loaded instruction file inside a combined project-instructions
/// snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstructionSource {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub byte_len: usize,
}

/// One-shot snapshot of loaded project instructions. Stored on `App` and
/// `NonInteractiveRunner` so the per-turn auto-reload check has
/// something to compare against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoadedInstructions {
    /// Primary absolute path the content was read from. Kept for
    /// compatibility with older renderer/status code; `sources`
    /// carries the full set.
    pub path: PathBuf,
    /// File body, possibly truncated. The truncation marker is
    /// appended in-place so the model sees the elision.
    pub content: String,
    /// mtime at last read — compared against the next `stat()` to
    /// decide whether to re-read.
    pub mtime: SystemTime,
    /// Original file size on disk (before any truncation).
    pub byte_len: usize,
    /// True when the file was larger than `MAX_INSTRUCTIONS_BYTES`
    /// and the content was clipped + marker appended.
    pub truncated: bool,
    /// All files that contributed to `content`.
    pub sources: Vec<InstructionSource>,
}

/// Where a memory lives. The *directory* is authoritative; the frontmatter
/// `scope` field is advisory/portable metadata so a hand-moved file is still
/// classified by its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemoryScope {
    Global,
    ProjectPrivate,
    ProjectShared,
}

/// One memory file's index entry. Deliberately holds no body — the full fact
/// is read on demand via `read_file` on `path`, keeping the always-loaded
/// snapshot small.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: MemoryScope,
    pub mtime: SystemTime,
}

/// Snapshot of all loaded memory across scopes plus the rendered index block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoadedMemory {
    pub entries: Vec<MemoryEntry>,
    /// Pre-rendered `# Memory` block injected into the prompt (capped).
    pub index: String,
    /// True when the index exceeded the cap and was clipped.
    pub truncated: bool,
}

/// Where a skill was discovered — also its precedence class (project wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `<git-root>/.mermaid/skills/<name>/SKILL.md` — shared with the team.
    Project,
    /// `<config_dir>/skills/<name>/SKILL.md` — this machine's user.
    User,
    /// Declared by an enabled plugin's manifest `skills` list.
    Plugin,
}

/// One discovered skill: index metadata plus the absolute SKILL.md path the
/// model reads on activation.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// Frontmatter `name:`, falling back to the skill's directory name.
    pub name: String,
    /// Frontmatter `description:`, falling back to the first body line.
    pub description: String,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
    /// Discovery origin (and precedence class).
    pub source: SkillSource,
}

/// Snapshot of all discovered skills plus the pre-rendered index block that
/// `build_chat_request` injects into the instructions suffix.
#[derive(Debug, Clone)]
pub struct LoadedSkills {
    /// Deduplicated entries in precedence order (project, user, plugin).
    pub entries: Vec<SkillEntry>,
    /// The rendered `# Skills` block (capped; see `render_index`).
    pub index: String,
}
