//! Loading, layering, merging and persisting [`mermaid_domain::Config`].
//!
//! The types themselves live in `src/domain/config.rs` — see that module for
//! why. This half is the impure one: it reads files, walks the layer cascade
//! (defaults < user < project < session flags) and writes back.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

use mermaid_model::constants::LEGACY_DEFAULT_MAX_TOKENS;
use mermaid_model::models::ReasoningLevel;

use mermaid_domain::config::*;

/// Remove the `profiles` table from a raw user-config table and return it
/// (empty when absent). `[profiles.<name>]` overlays must NEVER reach
/// `Config` deserialization — they are a container of layer tables, not
/// config keys — so every user-file read excises them before
/// `finalize_config` (which would otherwise warn about unknown keys) and
/// before any safety baseline is computed.
fn take_profiles(table: &mut toml::Table) -> toml::Table {
    match table.remove("profiles") {
        Some(toml::Value::Table(profiles)) => profiles,
        // A non-table `profiles` key is malformed; drop it (the profile
        // lookup errors clearly when one was requested).
        _ => toml::Table::new(),
    }
}

/// Resolve `--profile <name>` against the user file's excised `[profiles.*]`
/// table: the named overlay as a `Profile` layer, or a hard error naming the
/// available profiles (sorted).
fn resolve_profile_layer(
    profiles: &toml::Table,
    name: &str,
    config_path: &std::path::Path,
) -> Result<LayerSource> {
    match profiles.get(name) {
        Some(toml::Value::Table(overlay)) => Ok(LayerSource {
            layer: ConfigLayer::Profile,
            origin: format!("profile:{} ({})", name, config_path.display()),
            table: overlay.clone(),
        }),
        Some(_) => anyhow::bail!(
            "config profile '{}' is not a table; define it as [profiles.{}] in {}",
            name,
            name,
            config_path.display()
        ),
        None => {
            let mut available: Vec<&str> = profiles.keys().map(String::as_str).collect();
            available.sort_unstable();
            if available.is_empty() {
                anyhow::bail!(
                    "no config profiles defined; add [profiles.{}] to {}",
                    name,
                    config_path.display()
                );
            }
            anyhow::bail!(
                "unknown config profile '{}'; available: {}",
                name,
                available.join(", ")
            )
        },
    }
}

/// Load the user-scope configuration (defaults + the user file, no project or
/// session layers). This is the view persistence baselines, the daemon, and
/// runtime re-reads use — anything that must not observe another repo's
/// project config or a one-off CLI flag.
pub fn load_config() -> Result<Config> {
    let config_path = get_config_path()?;
    let mut table = read_config_table(&config_path)?;
    migrate_legacy_max_tokens(&mut table);
    migrate_legacy_model_profiles(&mut table);
    let _ = take_profiles(&mut table);
    Ok(finalize_config(table)?.0)
}

/// A completed layered load: the merged config plus the messages the startup
/// path surfaces.
pub struct LayeredLoad {
    /// The merged, typed configuration.
    pub config: Config,
    /// Layer-attributed unknown-key and project-sanitizer warnings.
    pub warnings: Vec<String>,
    /// Informational lines (e.g. "using project config …").
    pub notices: Vec<String>,
}

/// Load the full layered configuration:
/// defaults < user file < project file < session flags.
/// `cwd` locates the project layer (`<git-root>/.mermaid/config.toml`,
/// sanitized + safety-clamped); pass `None` to skip it (daemon, tests).
pub fn load_layered_config(
    cwd: Option<&std::path::Path>,
    flags: &SessionFlags,
) -> Result<LayeredLoad> {
    let config_path = get_config_path()?;
    let mut user_table = read_config_table(&config_path)?;
    migrate_legacy_max_tokens(&mut user_table);
    migrate_legacy_model_profiles(&mut user_table);
    // Excise [profiles.*] BEFORE anything deserializes the user table (the
    // safety baseline below and finalize_config's unknown-key scan).
    let profiles = take_profiles(&mut user_table);
    let mut layers = vec![LayerSource {
        layer: ConfigLayer::User,
        origin: config_path.display().to_string(),
        table: user_table.clone(),
    }];
    let mut sanitizer_warnings = Vec::new();
    let mut notices = Vec::new();
    if let Some(name) = flags.profile.as_deref() {
        let layer = resolve_profile_layer(&profiles, name, &config_path)?;
        notices.push(format!(
            "using config profile '{}' (from {})",
            name,
            config_path.display()
        ));
        layers.push(layer);
    }
    if let Some(cwd) = cwd {
        // The tighten-only safety clamp compares against the user-scope
        // (defaults + user file) values.
        let base_safety = finalize_config(user_table)?.0.safety;
        let (layer, warnings, notice) =
            super::project_config::load_project_layer(cwd, &base_safety);
        sanitizer_warnings.extend(warnings);
        notices.extend(notice);
        if let Some(layer) = layer {
            layers.push(layer);
        }
    }
    layers.push(LayerSource {
        layer: ConfigLayer::Session,
        origin: "command line".to_string(),
        table: session_flags_table(flags)?,
    });
    let (mut config, unknown_key_warnings) = merge_layers(layers)?;
    config.active_profile = flags.profile.clone();
    // Sanitizer warnings first: they explain keys that will also be absent
    // from the merged result.
    sanitizer_warnings.extend(unknown_key_warnings);
    Ok(LayeredLoad {
        config,
        warnings: sanitizer_warnings,
        notices,
    })
}

/// The project-scoped view (defaults + user + project, NO session flags) for
/// runtime re-reads keyed to a workdir — e.g. the memory settings consulted
/// per operation. Never fails and never prints; warnings/notices were already
/// surfaced by the startup load.
#[must_use]
pub fn load_project_scoped_config(cwd: &std::path::Path) -> Config {
    fn load(cwd: &std::path::Path) -> Result<Config> {
        let config_path = get_config_path()?;
        let mut user_table = read_config_table(&config_path)?;
        migrate_legacy_max_tokens(&mut user_table);
        migrate_legacy_model_profiles(&mut user_table);
        let _ = take_profiles(&mut user_table);
        let base_safety = finalize_config(user_table.clone())?.0.safety;
        let mut layers = vec![LayerSource {
            layer: ConfigLayer::User,
            origin: config_path.display().to_string(),
            table: user_table,
        }];
        let (layer, _warnings, _notice) =
            super::project_config::load_project_layer(cwd, &base_safety);
        if let Some(layer) = layer {
            layers.push(layer);
        }
        Ok(merge_layers(layers)?.0)
    }
    load(cwd).unwrap_or_default()
}

/// Like [`load_config`] (user scope, no session flags) but never fails: on a
/// malformed config, warn on stderr (secret-redacted, #F13) and fall back to
/// defaults (#111). For standalone subcommands that only read user settings.
#[must_use]
pub fn load_config_or_warn() -> Config {
    load_config().unwrap_or_else(|e| {
        eprintln!(
            "mermaid: {}",
            mermaid_model::utils::redact_secrets(&format!("{e:#}"))
        );
        Config::default()
    })
}

/// Read and parse one layer's TOML file; a missing file is an empty table.
pub(crate) fn read_config_table(path: &std::path::Path) -> Result<toml::Table> {
    if !path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str::<toml::Table>(&raw).with_context(|| {
        format!(
            "Failed to parse {}. Run 'mermaid init' to regenerate.",
            path.display()
        )
    })
}

/// Deep-merge the layers in order (later wins) and deserialize the result
/// once. Unknown-key warnings are collected per layer so each names the file
/// (or flag set) that actually contains the typo.
pub(crate) fn merge_layers(layers: Vec<LayerSource>) -> Result<(Config, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut merged = toml::Table::new();
    for layer in layers {
        collect_layer_warnings(&layer, &mut warnings);
        deep_merge(&mut merged, layer.table);
    }
    let (config, _) = finalize_config(merged)?;
    Ok((config, warnings))
}

/// Run one layer's table through `serde_ignored` purely for warning
/// attribution. A layer that fails to deserialize on its own contributes no
/// warnings — the authoritative merged deserialize in `merge_layers` surfaces
/// any real error (and a later layer may legitimately fix an earlier one's
/// value).
fn collect_layer_warnings(layer: &LayerSource, warnings: &mut Vec<String>) {
    let mut ignored = Vec::new();
    let result: Result<Config, _> =
        serde_ignored::deserialize(toml::Value::Table(layer.table.clone()), |path| {
            ignored.push(path.to_string())
        });
    if result.is_ok() {
        for path in ignored {
            warnings.push(format!(
                "unknown config key '{path}' in {} ({}) — check for a typo",
                layer.layer.name(),
                layer.origin
            ));
        }
    }
}

/// Recursively merge `overlay` into `base`: tables merge key-by-key, while
/// scalars and arrays replace wholesale (arrays are atomic values here — an
/// element-wise merge could never express removing an entry). A kind conflict
/// (table over scalar or vice versa) resolves to the overlay's value.
fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge(base_table, overlay_table);
            },
            (_, value) => {
                base.insert(key, value);
            },
        }
    }
}

/// One-time migration for the AUTO output-budget change. Existing config files
/// froze the old `default_model.max_tokens = 4096` default to disk (`save_config`
/// serializes every field), which would otherwise pin the stale cap forever.
/// Coerce that legacy value to `0` (AUTO) so upgraded users get the model-scaled
/// budget. Applied to the on-disk table *before* CLI overrides, so an explicit
/// `-c default_model.max_tokens=4096` still wins. The only unpreserved case is a
/// user who hand-wrote exactly `4096` in config.toml — an unusual deliberate
/// value, and AUTO is the better default regardless.
fn migrate_legacy_max_tokens(table: &mut toml::Table) {
    if let Some(dm) = table
        .get_mut("default_model")
        .and_then(|v| v.as_table_mut())
        && dm.get("max_tokens").and_then(|v| v.as_integer())
            == Some(LEGACY_DEFAULT_MAX_TOKENS as i64)
    {
        dm.insert("max_tokens".to_string(), toml::Value::Integer(0));
    }
}

/// Migrate the pre-profiles `[model_profiles]` table to its new name,
/// `[model_aliases]` (the `profile` name now belongs to `--profile` config
/// overlays). Runs wherever `migrate_legacy_max_tokens` runs: config loads
/// stop warning immediately, and the next persist converges the file on
/// disk. A file that somehow has BOTH tables keeps `model_aliases`.
fn migrate_legacy_model_profiles(table: &mut toml::Table) {
    if table.contains_key("model_aliases") {
        table.remove("model_profiles");
        return;
    }
    if let Some(profiles) = table.remove("model_profiles") {
        table.insert("model_aliases".to_string(), profiles);
    }
}

/// Deserialize a (possibly merged) config `Table` into `Config`, collecting the
/// dotted paths of any keys `Config` doesn't recognize so the caller can warn.
/// An empty table yields `Config::default()` (every field is `#[serde(default)]`).
fn finalize_config(table: toml::Table) -> Result<(Config, Vec<String>)> {
    let mut ignored = Vec::new();
    let mut config: Config = serde_ignored::deserialize(toml::Value::Table(table), |path| {
        ignored.push(path.to_string());
    })
    .context("Failed to interpret configuration. Run 'mermaid init' to regenerate.")?;
    // `plan` is a live session mode, not a persistent default: entering it
    // allocates a plan file, which config loading has no session to do it for.
    // `safety.mode = "plan"` would otherwise start a session that reports
    // "planning" with no plan to write. Fall back to the default and let
    // `/plan`, `/safety plan`, or Shift+Tab do the real thing. It is also what
    // `mode_after_plan` reads, so this must never be `plan` itself.
    if config.safety.mode.is_planning() {
        config.safety.mode = SafetyConfig::default().mode;
        ignored.push(
            "safety.mode (plan is entered with /plan or Shift+Tab, not configured)".to_string(),
        );
    }
    Ok((config, ignored))
}

/// Apply repeatable `-c KEY=VALUE` overrides onto a config table. `KEY` is a
/// dotted path (`default_model.model`); `VALUE` is parsed as a TOML scalar so
/// `true`/`3`/`"x"` keep their types, with a bare word treated as a string.
fn apply_cli_overrides(table: &mut toml::Table, overrides: &[String]) -> Result<()> {
    for raw in overrides {
        let (key, val) = raw
            .split_once('=')
            .with_context(|| format!("invalid -c override '{raw}' (expected KEY=VALUE)"))?;
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("invalid -c override '{raw}' (empty key)");
        }
        deep_set(table, key, parse_override_value(val.trim()))?;
    }
    Ok(())
}

/// Parse an override value as a standalone TOML value, falling back to a plain
/// string when it isn't valid TOML on its own (e.g. `ollama/qwen`).
fn parse_override_value(s: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("x = {s}"))
        .ok()
        .and_then(|t| t.get("x").cloned())
        .unwrap_or_else(|| toml::Value::String(s.to_string()))
}

/// Set a dotted `key` path in `table` to `value`, creating intermediate
/// tables. Dotted-path parsing means a `-c` override cannot address a map key
/// that itself contains a dot (e.g. a `reasoning_per_model` model id) — a
/// documented syntax limitation; internal persists use
/// [`deep_set_segments`] directly and are immune.
fn deep_set(table: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    deep_set_segments(table, &parts, value).with_context(|| format!("cannot set '{key}'"))
}

/// Set a pre-split `path` in `table` to `value`, creating intermediate tables.
/// Segments are literal keys — a segment containing a dot addresses exactly
/// that key (which dotted parsing cannot express).
fn deep_set_segments(table: &mut toml::Table, path: &[&str], value: toml::Value) -> Result<()> {
    let Some((leaf, parents)) = path.split_last() else {
        anyhow::bail!("empty config key path");
    };
    let mut cur = table;
    for part in parents {
        let next = cur
            .entry((*part).to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = next
            .as_table_mut()
            .with_context(|| format!("'{part}' is not a table"))?;
    }
    cur.insert((*leaf).to_string(), value);
    Ok(())
}

/// Remove a pre-split `path` from `table`. Returns whether a value was
/// actually removed. Never creates intermediate tables; a missing parent
/// simply means there was nothing to remove.
pub(crate) fn deep_remove_segments(table: &mut toml::Table, path: &[&str]) -> bool {
    let Some((leaf, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = table;
    for part in parents {
        match cur.get_mut(*part).and_then(|v| v.as_table_mut()) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    cur.remove(*leaf).is_some()
}

/// Like [`load_layered_config`] but never fails — the startup entry point.
/// On success, prints notices and layer-attributed warnings to stderr. On a
/// malformed layer, warns (secret-redacted, #F13) and degrades: the session
/// flags are re-applied over bare defaults so `--no-network`/`-c` survive a
/// corrupt user file rather than being silently dropped with it.
#[must_use]
pub fn load_layered_config_or_warn(cwd: Option<&std::path::Path>, flags: &SessionFlags) -> Config {
    match load_layered_config(cwd, flags) {
        Ok(load) => {
            for notice in &load.notices {
                eprintln!("mermaid: {notice}");
            }
            for warning in &load.warnings {
                eprintln!("mermaid: warning: {warning}");
            }
            load.config
        },
        Err(e) => {
            // A TOML parse error renders the offending source line, which can be
            // a secret-bearing one (`extra_headers`/`env`/`api_key_env`); scrub
            // credential-shaped content before it reaches stderr (#F13).
            eprintln!(
                "mermaid: {}",
                mermaid_model::utils::redact_secrets(&format!("{e:#}"))
            );
            session_flags_table(flags)
                .ok()
                .and_then(|table| finalize_config(table).ok())
                .map(|(config, _)| config)
                .unwrap_or_default()
        },
    }
}

/// Get the path to the single config file
pub fn get_config_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("config.toml"))
}

/// Get the configuration directory
pub fn get_config_dir() -> Result<PathBuf> {
    if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
        let config_dir = proj_dirs.config_dir();
        std::fs::create_dir_all(config_dir)?;
        Ok(config_dir.to_path_buf())
    } else {
        // Fallback to home directory
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .context("Could not determine home directory")?;
        let config_dir = PathBuf::from(home).join(".config").join("mermaid");
        std::fs::create_dir_all(&config_dir)?;
        Ok(config_dir)
    }
}

/// Save a full configuration to file. Private on purpose: serializing the
/// whole typed `Config` freezes every default (and would freeze merged
/// project/session values) into the file, so the only legitimate callers are
/// `init_config` (writing pristine defaults to an absent file) and tests.
/// Runtime persistence goes through [`update_user_config_key`] /
/// [`remove_user_config_key`], which rewrite only their own keys.
fn save_config(config: &Config, path: Option<PathBuf>) -> Result<()> {
    let path = if let Some(p) = path {
        p
    } else {
        get_config_dir()?.join("config.toml")
    };
    write_config_bytes(&path, toml::to_string_pretty(config)?.as_bytes())
}

/// Write raw config bytes atomically and owner-only.
///
/// The config can carry literal secrets — `mcp_servers[].env`,
/// `mcp_servers[].args`, `mcp_servers[].headers`, and
/// `providers[].extra_headers` all accept inline credential values — so it
/// must not be left world-readable, and a crash
/// mid-write must not truncate it. Write atomically (temp → fsync → rename),
/// creating the temp 0600 on Unix so the renamed file is never even briefly
/// world-readable (this also tightens a pre-existing config, since the new
/// file replaces the old one). Windows relies on the per-user profile ACL.
fn write_config_bytes(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    mermaid_runtime::write_atomic_with_mode(path, bytes, 0o600)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    #[cfg(not(unix))]
    mermaid_runtime::write_atomic(path, bytes)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    Ok(())
}

/// Create a default configuration file if it doesn't exist
pub fn init_config() -> Result<()> {
    let config_file = get_config_path()?;

    if config_file.exists() {
        println!("Configuration already exists at: {}", config_file.display());
    } else {
        let default_config = Config::default();
        save_config(&default_config, Some(config_file.clone()))?;
        println!("Created configuration at: {}", config_file.display());
    }

    Ok(())
}

/// Serializes the read-modify-write persistence path. The `persist_*` helpers
/// run as concurrent detached tasks (dispatched by the effect runner) that all
/// load → mutate → save the same file; without a lock two quick toggles
/// (`/model` then Alt+T) can interleave their loads and lose one write. Held
/// only across the synchronous fs work — never across an `.await`.
static PERSIST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read the raw USER config table, apply `mutate`, and write it back — under
/// `PERSIST_LOCK` so concurrent persists can't clobber each other. Operating
/// on the raw table (never the merged typed `Config`) means a persist rewrites
/// only its own keys: unknown keys survive, defaults are not frozen in, and
/// project-layer or session-flag values can never leak into the user file.
/// A malformed file propagates the parse error rather than being overwritten
/// with defaults (#111).
fn update_user_config_table(mutate: impl FnOnce(&mut toml::Table) -> Result<()>) -> Result<()> {
    update_user_config_table_at(&get_config_path()?, mutate)
}

/// [`update_user_config_table`] against an explicit path (test seam).
fn update_user_config_table_at(
    path: &std::path::Path,
    mutate: impl FnOnce(&mut toml::Table) -> Result<()>,
) -> Result<()> {
    let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut table = read_config_table(path)?;
    // Converge the on-disk legacy output cap while we're rewriting anyway.
    migrate_legacy_max_tokens(&mut table);
    migrate_legacy_model_profiles(&mut table);
    mutate(&mut table)?;
    write_config_bytes(path, toml::to_string_pretty(&table)?.as_bytes())
}

/// Set one key (pre-split path segments, so map keys containing dots — e.g.
/// `reasoning_per_model."ollama/qwen3:8b"` — address correctly) in the USER
/// config file, leaving every other key untouched.
pub fn update_user_config_key(path: &[&str], value: toml::Value) -> Result<()> {
    update_user_config_table(|table| deep_set_segments(table, path, value))
}

/// Persist the whole `[plan]` table (the `/plan config` picker). Values the
/// user set through the picker are explicit choices, so writing them —
/// including ones that currently match defaults — is correct; unset Options
/// stay absent via `skip_serializing_if`.
pub fn persist_plan_config(plan: &PlanConfig) -> Result<()> {
    update_user_config_key(&["plan"], toml::Value::try_from(plan)?)
}

/// Remove one key (pre-split path segments) from the USER config file.
/// Returns whether the key existed.
pub fn remove_user_config_key(path: &[&str]) -> Result<bool> {
    let mut removed = false;
    update_user_config_table(|table| {
        removed = deep_remove_segments(table, path);
        Ok(())
    })?;
    Ok(removed)
}

/// Persist the last used model to the user config file.
pub fn persist_last_model(model: &str) -> Result<()> {
    update_user_config_key(&["last_used_model"], toml::Value::String(model.to_string()))
}

/// Persist the TUI theme choice (`/theme dark|light`).
pub fn persist_ui_theme(theme: ThemeChoice) -> Result<()> {
    update_user_config_key(
        &["ui", "theme"],
        toml::Value::String(theme.as_str().to_string()),
    )
}

/// Persist the user's default reasoning level. Used by the `/reasoning` slash
/// command and the Alt+T cycle handler so the choice survives across sessions.
pub fn persist_default_reasoning(level: ReasoningLevel) -> Result<()> {
    update_user_config_key(
        &["default_model", "reasoning"],
        toml::Value::try_from(level)?,
    )
}

/// Persist a reasoning level for a specific model ID
/// (e.g. `<provider>/<model>`). The TUI calls this from Alt+T,
/// `/reasoning <level>`, and the does-not-support-thinking auto-snap so
/// the choice sticks per-model rather than bleeding into other models on
/// next session start.
pub fn persist_reasoning_for_model(model_id: &str, level: ReasoningLevel) -> Result<()> {
    update_user_config_key(
        &["reasoning_per_model", model_id],
        toml::Value::try_from(level)?,
    )
}

/// Persist (or clear) a per-model Ollama `num_ctx` override. `Some(n)` sets it,
/// `None` removes the entry (returning that model to auto-fit).
pub fn persist_ollama_num_ctx_for_model(model_id: &str, num_ctx: Option<u32>) -> Result<()> {
    match num_ctx {
        Some(n) => update_user_config_key(
            &["ollama_num_ctx_per_model", model_id],
            toml::Value::Integer(i64::from(n)),
        ),
        None => remove_user_config_key(&["ollama_num_ctx_per_model", model_id]).map(|_| ()),
    }
}

/// Persist the Ollama RAM-offload toggle (`/context offload on|off`).
pub fn persist_ollama_allow_ram_offload(enabled: bool) -> Result<()> {
    update_user_config_key(
        &["ollama", "allow_ram_offload"],
        toml::Value::Boolean(enabled),
    )
}

/// Resolve which model to use: CLI arg > `last_used` > `[default_model]` > a
/// local Ollama model > a configured provider's `default_model`.
pub async fn resolve_model_id(cli_model: Option<&str>, config: &Config) -> anyhow::Result<String> {
    if let Some(model) = cli_model {
        if let Some(resolved) = resolve_model_alias(model, config)? {
            return Ok(resolved);
        }
        return Ok(model.to_string());
    }
    if let Some(last_model) = &config.last_used_model {
        if let Some(resolved) = resolve_model_alias(last_model, config)? {
            return Ok(resolved);
        }
        return Ok(last_model.clone());
    }
    if !config.default_model.provider.is_empty() && !config.default_model.name.is_empty() {
        return Ok(format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        ));
    }
    // Nothing pinned. Ollama is Mermaid's default backend, not a prerequisite:
    // prefer a local model when one is installed, then a remote provider the
    // user has given an explicit `default_model`, and only then give up — with
    // a message that offers both routes instead of demanding an Ollama install
    // from someone who set `ANTHROPIC_API_KEY` and never wanted local models.
    let local = crate::ollama::local_models(config).await;
    if let Some(first) = local.as_ref().and_then(|models| models.first()) {
        return Ok(format!("ollama/{first}"));
    }
    if let Some(model_id) = configured_provider_default_model(config) {
        return Ok(model_id);
    }
    Err(no_model_configured_error(config, local.is_some()))
}

/// A `[providers.<name>].default_model` belonging to a provider whose API key
/// resolves right now. It is a model id the user typed themselves, so using it
/// as the startup default requires no guess about which models a vendor
/// currently ships — Mermaid never invents model names.
fn configured_provider_default_model(config: &Config) -> Option<String> {
    for provider in crate::providers::configured_remote_providers(config) {
        let model = config
            .providers
            .get(&provider.name)
            .and_then(|entry| entry.default_model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some(model) = model else { continue };
        // The field holds a bare model name, but an id that already carries
        // its provider prefix (or an OpenRouter-style `vendor/model`) must not
        // be double-prefixed into `openrouter/openrouter/...`.
        if model.starts_with(&format!("{}/", provider.name)) {
            return Some(model.to_string());
        }
        return Some(format!("{}/{}", provider.name, model));
    }
    None
}

/// The startup error for "no model is configured yet".
///
/// Ollama is one of two ways to get a model, so this never tells a user who
/// already has a provider key that they must install it. `ollama_installed`
/// distinguishes "install Ollama" from "you have Ollama, pull a model".
fn no_model_configured_error(config: &Config, ollama_installed: bool) -> anyhow::Error {
    let providers = crate::providers::configured_remote_providers(config);
    let mut lines = vec!["No model configured yet.".to_string(), String::new()];

    if let Some(first) = providers.first() {
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        lines.push(format!("Remote providers ready: {}", names.join(", ")));
        lines.push("Name a model to use one, e.g.:".to_string());
        lines.push(format!("    mermaid --model {}/<model>", first.name));
        lines.push(
            "Mermaid remembers the last model you used, so --model is a one-time step; \
             `mermaid list` shows what is available."
                .to_string(),
        );
        lines.push(String::new());
        lines.push("Or pin one in config.toml:".to_string());
        lines.push(format!("    [providers.{}]", first.name));
        lines.push("    default_model = \"<model>\"".to_string());
    } else {
        lines.push(
            "For a remote model, set a provider key (ANTHROPIC_API_KEY, OPENAI_API_KEY,"
                .to_string(),
        );
        lines.push("GOOGLE_API_KEY, GROQ_API_KEY, OPENROUTER_API_KEY, …) and name a".to_string());
        lines.push("model: mermaid --model anthropic/<model>".to_string());
    }

    lines.push(String::new());
    if ollama_installed {
        lines.push("For a local model, pull one first: ollama pull qwen3:8b".to_string());
    } else {
        lines.push(
            "For local models, install Ollama (https://ollama.com/download), then: \
             ollama pull qwen3:8b"
                .to_string(),
        );
    }
    lines.push("`mermaid doctor` reports what is and isn't ready.".to_string());

    anyhow::anyhow!(lines.join("\n"))
}

fn resolve_model_alias(requested: &str, config: &Config) -> anyhow::Result<Option<String>> {
    let alias = requested.strip_prefix("alias:").unwrap_or(requested);
    if let Some(model) = config.model_aliases.get(alias) {
        anyhow::ensure!(
            !model.trim().is_empty(),
            "model alias `{alias}` is configured with an empty model id"
        );
        return Ok(Some(model.clone()));
    }
    if requested.starts_with("alias:") {
        anyhow::bail!("model alias `{alias}` is not configured; add it under [model_aliases]");
    }
    Ok(None)
}

/// Render `SessionFlags` as the `Session` layer's raw table.
///
/// A free function, not an inherent method: `SessionFlags` is defined in
/// `mermaid-domain` and this needs the merge helpers, which are behavior and
/// live here. The orphan rule makes that split explicit rather than optional.
///
/// `-c` overrides go in first; the dedicated flags deep-set on top of them,
/// preserving the ordering where `--no-network` beats
/// `-c safety.network=allow`.
pub(crate) fn session_flags_table(flags: &SessionFlags) -> Result<toml::Table> {
    let mut table = toml::Table::new();
    apply_cli_overrides(&mut table, &flags.overrides)?;
    if flags.deny_network {
        deep_set_segments(
            &mut table,
            &["safety", "network"],
            toml::Value::String("deny".into()),
        )?;
    }
    if flags.confine_fs {
        deep_set_segments(
            &mut table,
            &["safety", "filesystem"],
            toml::Value::String("project".into()),
        )?;
    }
    if let Some(n) = flags.max_tokens {
        deep_set_segments(
            &mut table,
            &["default_model", "max_tokens"],
            toml::Value::Integer(n as i64),
        )?;
    }
    if flags.allow_untrusted_tools {
        deep_set_segments(
            &mut table,
            &["safety", "allow_untrusted_headless_tools"],
            toml::Value::Boolean(true),
        )?;
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_runtime::SafetyMode;
    use std::collections::HashMap;

    #[test]
    fn legacy_default_max_tokens_migrates_to_auto() {
        // The frozen pre-AUTO default (4096) on disk is coerced to 0 = AUTO…
        let mut table: toml::Table =
            toml::from_str("[default_model]\nmax_tokens = 4096\n").unwrap();
        migrate_legacy_max_tokens(&mut table);
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);

        // …while any other explicit cap is preserved.
        let mut table: toml::Table =
            toml::from_str("[default_model]\nmax_tokens = 8192\n").unwrap();
        migrate_legacy_max_tokens(&mut table);
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 8192);

        // A config without the key is untouched (stays the 0 default).
        let mut table = toml::Table::new();
        migrate_legacy_max_tokens(&mut table);
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);
    }

    #[test]
    fn legacy_model_profiles_table_migrates_to_model_aliases() {
        // Loads stop warning immediately...
        let mut table: toml::Table =
            toml::from_str("[model_profiles]\nfast = \"ollama/qwen3:8b\"\n").unwrap();
        migrate_legacy_model_profiles(&mut table);
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.model_aliases["fast"], "ollama/qwen3:8b");
        assert!(ignored.is_empty(), "no unknown-key warning: {ignored:?}");
        // ...and a file with BOTH keeps the new table.
        let mut table: toml::Table =
            toml::from_str("[model_profiles]\nfast = \"old\"\n[model_aliases]\nfast = \"new\"\n")
                .unwrap();
        migrate_legacy_model_profiles(&mut table);
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.model_aliases["fast"], "new");
        assert!(ignored.is_empty());
        // ...and the persist path rewrites the key on disk.
        let dir = std::env::temp_dir().join("mermaid_test_model_profiles_migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[model_profiles]\nfast = \"ollama/x\"\n").unwrap();
        update_user_config_table_at(&path, |_| Ok(())).unwrap();
        let blob = std::fs::read_to_string(&path).unwrap();
        assert!(blob.contains("[model_aliases]"), "{blob}");
        assert!(!blob.contains("model_profiles"), "{blob}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_theme_deserializes_defaults_and_rejects_typos() {
        let config: Config = toml::from_str("[ui]\ntheme = \"light\"\n").unwrap();
        assert_eq!(config.ui.theme, ThemeChoice::Light);
        // Absent → dark, both from an empty file and from Config::default().
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.ui.theme, ThemeChoice::Dark);
        assert_eq!(Config::default().ui.theme, ThemeChoice::Dark);
        // Typos are a clear deserialize error, not a silent fallback.
        assert!(toml::from_str::<Config>("[ui]\ntheme = \"solarized\"\n").is_err());
    }

    #[test]
    fn finalize_config_flags_unknown_keys() {
        let table: toml::Table =
            toml::from_str("unknown_top = 1\n[default_model]\nmax_tokens = 512\nbogus = true\n")
                .unwrap();
        let (config, ignored) = finalize_config(table).expect("finalizes despite unknown keys");
        assert_eq!(config.default_model.max_tokens, 512);
        assert!(
            ignored.iter().any(|p| p == "unknown_top"),
            "got {ignored:?}"
        );
        assert!(
            ignored.iter().any(|p| p.contains("bogus")),
            "got {ignored:?}"
        );
    }

    #[test]
    fn cli_overrides_beat_file_and_create_nested_tables() {
        // Override beats the file value...
        let mut table: toml::Table = toml::from_str("[default_model]\nmax_tokens = 100\n").unwrap();
        apply_cli_overrides(&mut table, &["default_model.max_tokens=8192".to_string()]).unwrap();
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 8192);
        assert!(ignored.is_empty());
        // ...and creates a section absent from the file.
        let mut empty = toml::Table::new();
        apply_cli_overrides(&mut empty, &["default_model.max_tokens=256".to_string()]).unwrap();
        assert_eq!(
            finalize_config(empty).unwrap().0.default_model.max_tokens,
            256
        );
    }

    #[test]
    fn parse_override_value_keeps_toml_types_with_string_fallback() {
        assert_eq!(parse_override_value("true"), toml::Value::Boolean(true));
        assert_eq!(parse_override_value("42"), toml::Value::Integer(42));
        assert_eq!(
            parse_override_value("ollama/qwen"),
            toml::Value::String("ollama/qwen".to_string())
        );
    }

    #[test]
    fn cli_override_invalid_format_errors() {
        let mut table = toml::Table::new();
        assert!(apply_cli_overrides(&mut table, &["noequalssign".to_string()]).is_err());
        assert!(apply_cli_overrides(&mut table, &["=novalue".to_string()]).is_err());
    }

    #[test]
    fn deep_merge_recurses_tables_and_replaces_scalars_and_arrays() {
        let mut base: toml::Table = toml::from_str(
            "top = 1\n[ollama]\nhost = \"localhost\"\nport = 11434\n[safety]\noverrides = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let overlay: toml::Table =
            toml::from_str("[ollama]\nhost = \"gpu-box\"\n[safety]\noverrides = [\"c\"]\n")
                .unwrap();
        deep_merge(&mut base, overlay);
        // Sibling keys inside a merged table survive...
        assert_eq!(base["ollama"]["port"].as_integer(), Some(11434));
        // ...the overlaid scalar wins...
        assert_eq!(base["ollama"]["host"].as_str(), Some("gpu-box"));
        // ...arrays replace wholesale (no concat)...
        assert_eq!(base["safety"]["overrides"].as_array().unwrap().len(), 1);
        // ...and untouched top-level keys survive.
        assert_eq!(base["top"].as_integer(), Some(1));
    }

    #[test]
    fn deep_merge_overlay_wins_on_kind_conflict() {
        // Scalar over table and table over scalar both resolve to the overlay.
        let mut base: toml::Table = toml::from_str("[a]\nx = 1\nb = 2\n").unwrap();
        let overlay: toml::Table = toml::from_str("a = 5\n[b]\ny = 3\n").unwrap();
        deep_merge(&mut base, overlay);
        assert_eq!(base["a"].as_integer(), Some(5));
        assert_eq!(base["b"]["y"].as_integer(), Some(3));
    }

    #[test]
    fn merge_layers_precedence_and_layer_attributed_warnings() {
        let user: toml::Table = toml::from_str(
            "last_used_model = \"ollama/a\"\nuser_typo = 1\n[default_model]\nmax_tokens = 100\n",
        )
        .unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/b\"\nsession_typo = 2\n").unwrap();
        let (config, warnings) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "/tmp/user.toml".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Later layer wins; earlier layer's untouched keys survive.
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/b"));
        assert_eq!(config.default_model.max_tokens, 100);
        // Each unknown key names its own layer + origin.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("user_typo") && w.contains("user config (/tmp/user.toml)")),
            "got {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("session_typo") && w.contains("session flags")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn take_profiles_excises_and_tolerates_absence() {
        let mut table: toml::Table =
            toml::from_str("[profiles.fast.default_model]\ntemperature = 0.1\n").unwrap();
        let profiles = take_profiles(&mut table);
        assert!(table.is_empty(), "profiles must be excised: {table:?}");
        assert!(profiles.contains_key("fast"));
        // Absent -> empty, table untouched.
        let mut table: toml::Table = toml::from_str("last_used_model = \"x\"\n").unwrap();
        assert!(take_profiles(&mut table).is_empty());
        assert_eq!(table.len(), 1);
        // Malformed (non-table) -> dropped, empty result.
        let mut table: toml::Table = toml::from_str("profiles = 3\n").unwrap();
        assert!(take_profiles(&mut table).is_empty());
        assert!(table.is_empty());
    }

    #[test]
    fn resolve_profile_layer_errors_name_available_profiles() {
        let profiles: toml::Table = toml::from_str("[work]\n[fast]\n").unwrap();
        let path = std::path::Path::new("/tmp/config.toml");
        let err = resolve_profile_layer(&profiles, "nope", path).unwrap_err();
        assert!(err.to_string().contains("available: fast, work"), "{err}");
        // No profiles at all -> a distinct, actionable error.
        let err = resolve_profile_layer(&toml::Table::new(), "work", path).unwrap_err();
        assert!(
            err.to_string().contains("no config profiles defined"),
            "{err}"
        );
        // Non-table profile value -> hard error.
        let profiles: toml::Table = toml::from_str("work = 1\n").unwrap();
        let err = resolve_profile_layer(&profiles, "work", path).unwrap_err();
        assert!(err.to_string().contains("not a table"), "{err}");
        // Hit -> Profile layer with attributing origin.
        let profiles: toml::Table =
            toml::from_str("[work.default_model]\ntemperature = 0.2\n").unwrap();
        let layer = resolve_profile_layer(&profiles, "work", path).unwrap();
        assert_eq!(layer.layer, ConfigLayer::Profile);
        assert!(layer.origin.contains("profile:work"));
    }

    #[test]
    fn profile_layer_beats_user_loses_to_project_and_session() {
        let user: toml::Table = toml::from_str(
            "last_used_model = \"ollama/user\"\n[default_model]\ntemperature = 0.9\nmax_tokens = 100\n",
        )
        .unwrap();
        let profile: toml::Table = toml::from_str(
            "last_used_model = \"ollama/profile\"\n[default_model]\ntemperature = 0.1\nprofile_typo = 1\n",
        )
        .unwrap();
        let project: toml::Table = toml::from_str("[default_model]\ntemperature = 0.5\n").unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/session\"\n").unwrap();
        let (config, warnings) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "/tmp/user.toml".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Profile,
                origin: "profile:work (/tmp/user.toml)".to_string(),
                table: profile,
            },
            LayerSource {
                layer: ConfigLayer::Project,
                origin: "/repo/.mermaid/config.toml".to_string(),
                table: project,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Project beats profile; session beats everything; profile beats user
        // where later layers are silent.
        assert_eq!(config.default_model.temperature, 0.5);
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/session"));
        assert_eq!(config.default_model.max_tokens, 100);
        // Unknown keys inside the profile attribute to it.
        assert!(
            warnings.iter().any(|w| w.contains("profile_typo")
                && w.contains("config profile (profile:work (/tmp/user.toml))")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn persists_never_touch_profile_tables() {
        let dir = std::env::temp_dir().join("mermaid_test_profiles_persist");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[profiles.fast.default_model]\ntemperature = 0.1\n\n[safety]\nmode = \"ask\"\n",
        )
        .expect("seed");

        update_user_config_table_at(&path, |table| {
            deep_set_segments(
                table,
                &["safety", "mode"],
                toml::Value::String("auto".to_string()),
            )
        })
        .expect("persist");

        let table: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(table["safety"]["mode"].as_str(), Some("auto"));
        // The overlay table survives persists byte-for-byte semantically.
        assert_eq!(
            table["profiles"]["fast"]["default_model"]["temperature"].as_float(),
            Some(0.1)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_flags_table_maps_each_flag() {
        let flags = SessionFlags {
            overrides: vec!["web.searxng_url=\"http://x:1\"".to_string()],
            deny_network: true,
            confine_fs: true,
            max_tokens: Some(512),
            allow_untrusted_tools: true,
            profile: None,
        };
        let (config, _) = finalize_config(session_flags_table(&flags).unwrap()).unwrap();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
        assert_eq!(config.safety.filesystem, FilesystemPolicy::Project);
        assert_eq!(config.default_model.max_tokens, 512);
        assert!(config.safety.allow_untrusted_headless_tools);
        assert_eq!(config.web.searxng_url, "http://x:1");
    }

    #[test]
    fn session_dedicated_flags_beat_dash_c() {
        // `--no-network` wins over a contradictory `-c safety.network=allow`
        // (the dedicated flags deep-set after the -c overrides).
        let flags = SessionFlags {
            overrides: vec!["safety.network=allow".to_string()],
            deny_network: true,
            ..Default::default()
        };
        let (config, _) = finalize_config(session_flags_table(&flags).unwrap()).unwrap();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
    }

    #[test]
    fn corrupt_layer_yields_no_warnings_but_merged_error_surfaces() {
        // A layer that doesn't deserialize on its own contributes no warnings…
        let bad: toml::Table = toml::from_str("[safety]\nmode = 42\n").unwrap();
        let mut warnings = Vec::new();
        collect_layer_warnings(
            &LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad.clone(),
            },
            &mut warnings,
        );
        assert!(warnings.is_empty());
        // …and the merged deserialize is what errors…
        assert!(
            merge_layers(vec![LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad.clone(),
            }])
            .is_err()
        );
        // …unless a later layer fixes the value (session repairing a bad file).
        let fix: toml::Table = toml::from_str("[safety]\nmode = \"ask\"\n").unwrap();
        let (config, _) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: fix,
            },
        ])
        .expect("later layer repairs the earlier one");
        assert_eq!(config.safety.mode, SafetyMode::Ask);
    }

    #[test]
    fn project_layer_beats_user_and_loses_to_session() {
        let user: toml::Table = toml::from_str("last_used_model = \"ollama/user\"\n").unwrap();
        let project: toml::Table = toml::from_str(
            "last_used_model = \"ollama/project\"\n[default_model]\nreasoning = \"low\"\n",
        )
        .unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/session\"\n").unwrap();
        let (config, _) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "user".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Project,
                origin: "project".to_string(),
                table: project,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Session beats project beats user for the contested key…
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/session"));
        // …while the project's uncontested key lands.
        assert_eq!(config.default_model.reasoning, ReasoningLevel::Low);
    }

    #[test]
    fn session_flags_survive_corrupt_user_layer_fallback() {
        // The or_warn fallback re-applies the session flags over bare defaults;
        // pin the exact expression it uses.
        let flags = SessionFlags {
            deny_network: true,
            ..Default::default()
        };
        let config = session_flags_table(&flags)
            .ok()
            .and_then(|table| finalize_config(table).ok())
            .map(|(config, _)| config)
            .unwrap_or_default();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
    }

    #[test]
    fn deep_set_segments_addresses_keys_containing_dots() {
        // A model id with dots must be ONE key, which dotted parsing cannot
        // express — the latent bug the segment API fixes.
        let mut table = toml::Table::new();
        deep_set_segments(
            &mut table,
            &["reasoning_per_model", "gemini/gemini-2.5-pro"],
            toml::Value::String("high".to_string()),
        )
        .unwrap();
        let (config, ignored) = finalize_config(table).unwrap();
        assert!(ignored.is_empty(), "got {ignored:?}");
        assert_eq!(
            config.reasoning_per_model.get("gemini/gemini-2.5-pro"),
            Some(&ReasoningLevel::High)
        );
    }

    #[test]
    fn deep_remove_segments_removes_leaf_only() {
        let mut table: toml::Table =
            toml::from_str("[ollama_num_ctx_per_model]\n\"ollama/a\" = 1\n\"ollama/b\" = 2\n")
                .unwrap();
        assert!(deep_remove_segments(
            &mut table,
            &["ollama_num_ctx_per_model", "ollama/a"]
        ));
        // Sibling survives; parent table survives; missing keys report false.
        assert_eq!(
            table["ollama_num_ctx_per_model"]["ollama/b"].as_integer(),
            Some(2)
        );
        assert!(!deep_remove_segments(
            &mut table,
            &["ollama_num_ctx_per_model", "ollama/a"]
        ));
        assert!(!deep_remove_segments(&mut table, &["nope", "x"]));
    }

    #[test]
    fn update_user_config_table_preserves_unknown_keys() {
        let dir = std::env::temp_dir().join("mermaid_test_config_targeted_persist");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        // A file with an unknown key (maybe from a newer mermaid) and one known
        // setting the persist must not disturb.
        std::fs::write(
            &path,
            "future_key = \"kept\"\nlast_used_model = \"ollama/old\"\n\n[ollama]\nport = 12345\n",
        )
        .expect("seed");

        update_user_config_table_at(&path, |table| {
            deep_set_segments(
                table,
                &["last_used_model"],
                toml::Value::String("ollama/new".to_string()),
            )
        })
        .expect("persist");

        let blob = std::fs::read_to_string(&path).expect("read back");
        let table: toml::Table = toml::from_str(&blob).expect("parse back");
        // The targeted key changed…
        assert_eq!(table["last_used_model"].as_str(), Some("ollama/new"));
        // …the unknown key survived (typed round-trips would have dropped it)…
        assert_eq!(table["future_key"].as_str(), Some("kept"));
        // …and no defaults were frozen in (only the keys that were there).
        assert!(!blob.contains("safety"), "defaults must not be frozen in");
        assert_eq!(table["ollama"]["port"].as_integer(), Some(12345));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_tool_allowed_honors_enabled_and_disabled() {
        // Default (both empty) allows everything.
        let cfg = McpServerConfig::default();
        assert!(cfg.tool_allowed("anything"));
        // enabled_tools acts as an allowlist.
        let cfg = McpServerConfig {
            enabled_tools: vec!["read".into(), "search".into()],
            ..Default::default()
        };
        assert!(cfg.tool_allowed("read"));
        assert!(!cfg.tool_allowed("write"));
        // disabled_tools wins over enabled_tools.
        let cfg = McpServerConfig {
            enabled_tools: vec!["read".into(), "write".into()],
            disabled_tools: vec!["write".into()],
            ..Default::default()
        };
        assert!(cfg.tool_allowed("read"));
        assert!(!cfg.tool_allowed("write"));
    }

    #[test]
    fn mcp_transport_kind_requires_exactly_one_of_command_and_url() {
        // command-only → stdio.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.transport_kind().unwrap(), TransportKind::Stdio);
        // url-only → http.
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.transport_kind().unwrap(), TransportKind::Http);
        // Both set → error.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        assert!(
            cfg.transport_kind()
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
        // Neither set → error.
        let cfg = McpServerConfig::default();
        assert!(
            cfg.transport_kind()
                .unwrap_err()
                .to_string()
                .contains("neither")
        );
    }

    #[test]
    fn mcp_transport_kind_gates_url_scheme() {
        let with_url = |url: &str| McpServerConfig {
            url: Some(url.to_string()),
            ..Default::default()
        };
        // https anywhere is fine; http only to loopback (plaintext to a
        // routable host would leak auth headers).
        assert!(
            with_url("https://mcp.example.com/x")
                .transport_kind()
                .is_ok()
        );
        assert!(
            with_url("http://localhost:8080/mcp")
                .transport_kind()
                .is_ok()
        );
        assert!(
            with_url("http://127.0.0.1:8080/mcp")
                .transport_kind()
                .is_ok()
        );
        assert!(with_url("http://192.168.1.5/mcp").transport_kind().is_err());
        assert!(with_url("ftp://example.com/mcp").transport_kind().is_err());
        assert!(with_url("not a url").transport_kind().is_err());
    }

    #[test]
    fn mcp_server_config_debug_masks_header_values() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer sk-secret".to_string());
        let mut env_headers = HashMap::new();
        env_headers.insert("X-Api-Key".to_string(), "MY_TOKEN_VAR".to_string());
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            headers,
            env_headers,
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("sk-secret"), "{rendered}");
        assert!(rendered.contains("Authorization"), "{rendered}");
        // env_headers values are env var NAMES, safe to render.
        assert!(rendered.contains("MY_TOKEN_VAR"), "{rendered}");
    }

    #[test]
    fn mcp_url_config_round_trips_through_toml_without_command() {
        // `mermaid add --url` persists via toml::Value::try_from; a bare None
        // url or a forced empty `command` key would break that round-trip.
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        let blob = toml::to_string(&toml::Value::try_from(&cfg).unwrap()).unwrap();
        assert!(
            !blob.contains("command"),
            "empty command must be omitted: {blob}"
        );
        let back: McpServerConfig = toml::from_str(&blob).unwrap();
        assert_eq!(back.url.as_deref(), Some("https://example.com/mcp"));
        assert!(back.command.is_empty());
        // And a stdio config must not serialize a `url` key at all.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            ..Default::default()
        };
        let blob = toml::to_string(&toml::Value::try_from(&cfg).unwrap()).unwrap();
        assert!(!blob.contains("url"), "{blob}");
    }

    /// Configs persisted before Step 4 don't have a `reasoning` field on
    /// `[default_model]`. Loading them must succeed and yield the
    /// `Medium` default — otherwise existing user configs break on
    /// upgrade.
    #[test]
    fn model_settings_deserializes_without_reasoning_field() {
        let toml_blob = r#"
            provider = "ollama"
            name = "qwen3-coder:30b"
            temperature = 0.7
            max_tokens = 4096
        "#;
        let settings: ModelSettings = toml::from_str(toml_blob).expect("backward compat");
        assert_eq!(settings.reasoning, ReasoningLevel::Medium);
        assert_eq!(settings.provider, "ollama");
    }

    #[test]
    fn model_settings_round_trips_reasoning_high() {
        let original = ModelSettings {
            provider: "anthropic".to_string(),
            name: "claude-sonnet-4-6".to_string(),
            temperature: 0.5,
            max_tokens: 8192,
            reasoning: ReasoningLevel::High,
        };
        let toml_blob = toml::to_string(&original).expect("serialize");
        let back: ModelSettings = toml::from_str(&toml_blob).expect("deserialize");
        assert_eq!(back.reasoning, ReasoningLevel::High);
        assert_eq!(back.name, "claude-sonnet-4-6");
    }

    #[test]
    fn agents_config_defaults_and_parses_custom_types() {
        // Absent section → defaults (20-minute timeout, no custom types).
        let config: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(config.agents.timeout_secs, 1200);
        assert!(config.agents.types.is_empty());

        let config: Config = toml::from_str(
            r#"
[agents]
timeout_secs = 300

[agents.types.scout]
tools = ["read_file", "execute_command"]
safety = "read_only"
preamble = "You are a scout."
model = "ollama/qwen3:8b"
"#,
        )
        .expect("agents section parses");
        assert_eq!(config.agents.timeout_secs, 300);
        let scout = &config.agents.types["scout"];
        assert_eq!(
            scout.tools.as_deref(),
            Some(&["read_file".to_string(), "execute_command".to_string()][..])
        );
        assert_eq!(scout.safety.as_deref(), Some("read_only"));
        assert_eq!(scout.model.as_deref(), Some("ollama/qwen3:8b"));
    }

    #[test]
    fn configured_model_alias_resolves_explicit_prefix() {
        let mut config = Config::default();
        config
            .model_aliases
            .insert("fast".to_string(), "ollama/qwen3-coder:14b".to_string());
        assert_eq!(
            resolve_model_alias("fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
        assert_eq!(
            resolve_model_alias("alias:fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
    }

    #[test]
    fn alias_prefix_requires_configuration() {
        let config = Config::default();
        assert!(resolve_model_alias("alias:vision", &config).is_err());
        assert_eq!(resolve_model_alias("vision", &config).unwrap(), None);
    }

    /// `persist_default_reasoning` writes to the real config path, so
    /// this test goes through `save_config(_, Some(path))` directly to
    /// avoid clobbering the user's actual `~/.config/mermaid/config.toml`.
    /// Uses `std::env::temp_dir` (matching the pattern in
    /// `session::conversation` and `utils::logger`) — no external
    /// `tempfile` crate dependency.
    #[test]
    fn save_and_reload_preserves_reasoning_field() {
        let dir = std::env::temp_dir().join("mermaid_test_config_reasoning");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.default_model.provider = "ollama".to_string();
        cfg.default_model.name = "qwen3-coder:30b".to_string();
        cfg.default_model.reasoning = ReasoningLevel::Low;

        save_config(&cfg, Some(path.clone())).expect("save");

        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");
        assert_eq!(loaded.default_model.reasoning, ReasoningLevel::Low);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Per-model entries serialize as a TOML table with quoted keys (the
    /// model IDs contain `/`). This test verifies the round-trip works
    /// through both serialization and deserialization, matching what
    /// `persist_reasoning_for_model` would produce in real use.
    #[test]
    fn save_and_reload_preserves_reasoning_per_model_table() {
        let dir = std::env::temp_dir().join("mermaid_test_config_per_model_reasoning");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.reasoning_per_model.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            ReasoningLevel::High,
        );
        cfg.reasoning_per_model
            .insert("ollama/qwen3-coder:30b".to_string(), ReasoningLevel::Low);

        save_config(&cfg, Some(path.clone())).expect("save");

        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");
        assert_eq!(
            loaded
                .reasoning_per_model
                .get("anthropic/claude-sonnet-4-6"),
            Some(&ReasoningLevel::High)
        );
        assert_eq!(
            loaded.reasoning_per_model.get("ollama/qwen3-coder:30b"),
            Some(&ReasoningLevel::Low)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/context <n>` overrides round-trip through the per-model TOML table, and
    /// the offload toggle persists on `[ollama]`.
    #[test]
    fn save_and_reload_preserves_ollama_context_overrides() {
        let dir = std::env::temp_dir().join("mermaid_test_config_ollama_ctx");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.ollama_num_ctx_per_model
            .insert("ollama/ornith:9b".to_string(), 131_072);
        cfg.ollama.allow_ram_offload = true;
        cfg.ollama.max_auto_num_ctx = Some(65_536);

        save_config(&cfg, Some(path.clone())).expect("save");
        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");

        assert_eq!(
            loaded.ollama_num_ctx_per_model.get("ollama/ornith:9b"),
            Some(&131_072)
        );
        assert!(loaded.ollama.allow_ram_offload);
        assert_eq!(loaded.ollama.max_auto_num_ctx, Some(65_536));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Older configs have neither the per-model `num_ctx` table nor the new
    /// `[ollama]` keys; loading must default cleanly (empty map, offload off).
    #[test]
    fn config_deserializes_without_ollama_context_keys() {
        let toml_blob = r#"
[ollama]
host = "localhost"
port = 11434
"#;
        let cfg: Config = toml::from_str(toml_blob).expect("parse");
        assert!(cfg.ollama_num_ctx_per_model.is_empty());
        assert!(!cfg.ollama.allow_ram_offload);
        assert_eq!(cfg.ollama.max_auto_num_ctx, None);
        // Configs from before the auto-start knob default it ON — reviving a
        // dead local server is the out-of-the-box behavior.
        assert!(cfg.ollama.auto_start);
    }

    /// Configs from before Step 5b don't have a `reasoning_per_model`
    /// section. Loading them must succeed with an empty map — otherwise
    /// upgrade breaks every existing user.
    #[test]
    fn config_deserializes_without_reasoning_per_model() {
        let toml_blob = r#"
            last_used_model = "ollama/qwen3-coder:30b"

            [default_model]
            provider = "ollama"
            name = "qwen3-coder:30b"
            temperature = 0.7
            max_tokens = 4096
        "#;
        let cfg: Config = toml::from_str(toml_blob).expect("backward compat");
        assert!(cfg.reasoning_per_model.is_empty());
        assert!(!cfg.prompt.is_customized());
    }

    /// Config holds inline-secret-capable fields (`mcp_servers[].env`, `args`,
    /// `headers`, `providers[].extra_headers`), so it must be written
    /// owner-only rather than inheriting a world-readable umask.
    #[cfg(unix)]
    #[test]
    fn save_config_writes_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("mermaid_test_config_perms");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        // Pre-create a world-readable file to prove we also tighten existing.
        std::fs::write(&path, "stale").expect("seed");
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));

        save_config(&Config::default(), Some(path.clone())).expect("save");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be written owner-only");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_computer_use_auto_screenshot_on() {
        // An empty/legacy config must keep the auto-screenshot behavior (#98).
        let cfg: Config = toml::from_str("").expect("empty config");
        assert!(cfg.computer_use.auto_screenshot);
    }

    #[test]
    fn prompt_config_replaces_and_appends_without_persisting() {
        let mut cfg = Config::default();
        cfg.prompt.system_prompt = Some("base".to_string());
        cfg.prompt
            .append_system_prompt
            .push("extra instructions".to_string());

        assert_eq!(
            cfg.prompt.render_system_prompt("default"),
            "base\n\nextra instructions"
        );

        let blob = toml::to_string(&cfg).expect("serialize");
        assert!(!blob.contains("extra instructions"));
        let loaded: Config = toml::from_str(&blob).expect("deserialize");
        assert!(!loaded.prompt.is_customized());
    }

    /// An absent `[compaction]` section must reproduce the constants exactly —
    /// making the policy configurable must not change anyone's behavior.
    #[test]
    fn absent_compaction_section_matches_the_built_in_policy() {
        let c: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(
            c.compaction.policy(),
            mermaid_domain::CompactionPolicy::default(),
        );
    }

    #[test]
    fn compaction_settings_reach_the_policy() {
        let c: Config = toml::from_str(
            "[compaction]\n\
             auto_enabled = false\n\
             auto_threshold_percent = 60\n\
             tail_turns = 5\n\
             tail_token_budget = 12000\n\
             summary_max_tokens = 3000\n",
        )
        .expect("compaction section parses");
        let policy = c.compaction.policy();
        assert!(!policy.auto_enabled);
        assert_eq!(policy.auto_threshold_percent, 60);
        assert_eq!(policy.tail_turns, 5);
        assert_eq!(policy.tail_token_budget, 12_000);
        assert_eq!(policy.summary_max_tokens, 3_000);
        // Unset keys keep their defaults rather than zeroing out.
        let defaults = mermaid_domain::CompactionPolicy::default();
        assert_eq!(policy.tool_output_max_chars, defaults.tool_output_max_chars);
    }

    /// A hand-edited config degrades to the nearest workable value rather than
    /// putting compaction in a state where it silently cannot run.
    #[test]
    fn nonsense_compaction_settings_are_clamped() {
        let c: Config = toml::from_str(
            "[compaction]\n\
             auto_threshold_percent = 250\n\
             tail_turns = 0\n\
             tail_token_budget = 0\n\
             summary_max_tokens = 0\n\
             summarizer_input_token_budget = 0\n\
             tool_output_max_chars = 0\n\
             min_response_reserve_tokens = 50000\n\
             max_response_reserve_tokens = 1000\n",
        )
        .expect("config parses");
        let policy = c.compaction.policy();
        let defaults = mermaid_domain::CompactionPolicy::default();

        assert_eq!(policy.auto_threshold_percent, 100, "percent clamps to 100");
        assert_eq!(
            policy.tail_turns, 1,
            "a checkpoint needs a live turn after it"
        );
        // Zero would mean "no budget at all"; fall back rather than disable.
        assert_eq!(policy.tail_token_budget, defaults.tail_token_budget);
        assert_eq!(policy.summary_max_tokens, defaults.summary_max_tokens);
        assert_eq!(
            policy.summarizer_input_token_budget,
            defaults.summarizer_input_token_budget
        );
        assert_eq!(policy.tool_output_max_chars, defaults.tool_output_max_chars);

        // Swapped reserve bounds are ordered, not obeyed: `response_reserve`
        // clamps with `.max(min).min(max)`, so an inverted pair would return
        // the smaller value and under-reserve on every single turn.
        assert_eq!(policy.min_response_reserve_tokens, 1_000);
        assert_eq!(policy.max_response_reserve_tokens, 50_000);
        assert!(policy.min_response_reserve_tokens <= policy.max_response_reserve_tokens);
    }

    /// `auto_threshold_percent = 0` would compact on every single turn, before
    /// there is anything to compact.
    #[test]
    fn zero_compaction_threshold_clamps_up() {
        let c: Config =
            toml::from_str("[compaction]\nauto_threshold_percent = 0\n").expect("parses");
        assert_eq!(c.compaction.policy().auto_threshold_percent, 1);
    }

    #[test]
    fn plan_config_defaults_parse_and_do_not_freeze() {
        // Absent section: dialog on, nothing pinned.
        let c: Config = toml::from_str("").expect("empty config parses");
        assert!(!c.plan.auto_approve);
        assert!(c.plan.post_approve.is_none());
        // Explicit values parse.
        let c: Config = toml::from_str("[plan]\nauto_approve = true\npost_approve = \"start\"\n")
            .expect("plan section parses");
        assert!(c.plan.auto_approve);
        assert_eq!(c.plan.post_approve, Some(PlanPostApprove::Start));
        assert_eq!(
            toml::from_str::<Config>("[plan]\npost_approve = \"wait\"\n")
                .expect("wait parses")
                .plan
                .post_approve,
            Some(PlanPostApprove::Wait)
        );
        // The unset pin is never frozen into a saved config (Option +
        // skip_serializing_if), so a future default change still reaches
        // existing files.
        let blob = toml::to_string(&Config::default()).expect("serialize");
        assert!(!blob.contains("post_approve"));
    }

    /// Config with one remote provider carrying an explicit `default_model`.
    fn config_with_provider_default(provider: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            provider.to_string(),
            UserProviderConfig {
                default_model: Some(model.to_string()),
                ..Default::default()
            },
        );
        config
    }

    /// The whole point of the Ollama-optional path: a machine whose only
    /// backend is Anthropic must resolve a model without Ollama in the picture.
    #[test]
    fn provider_default_model_resolves_without_ollama() {
        let config = config_with_provider_default("anthropic", "claude-x");
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            assert_eq!(
                configured_provider_default_model(&config).as_deref(),
                Some("anthropic/claude-x")
            );
        });
    }

    /// An unconfigured provider's `default_model` is not a usable default —
    /// building it would fail on the missing key at the first request.
    #[test]
    fn provider_default_model_ignored_without_a_key() {
        let config = config_with_provider_default("anthropic", "claude-x");
        temp_env::with_vars([("ANTHROPIC_API_KEY", None::<&str>)], || {
            // The keyring is the machine's, so only assert the env-var half:
            // with no key in the environment there is nothing to prefer.
            if mermaid_model::utils::provider_key_source("anthropic", "ANTHROPIC_API_KEY", None)
                == "none"
            {
                assert_eq!(configured_provider_default_model(&config), None);
            }
        });
    }

    /// OpenRouter ids are `vendor/model`, which must be prefixed once, not
    /// twice — and an id that already names its provider is left alone.
    #[test]
    fn provider_default_model_is_prefixed_exactly_once() {
        temp_env::with_vars([("OPENROUTER_API_KEY", Some("sk-test"))], || {
            let vendor_model = config_with_provider_default("openrouter", "z-ai/glm-5.2");
            assert_eq!(
                configured_provider_default_model(&vendor_model).as_deref(),
                Some("openrouter/z-ai/glm-5.2")
            );
            let already_prefixed =
                config_with_provider_default("openrouter", "openrouter/z-ai/glm-5.2");
            assert_eq!(
                configured_provider_default_model(&already_prefixed).as_deref(),
                Some("openrouter/z-ai/glm-5.2")
            );
        });
    }

    /// The regression this replaced: startup used to end at "Ollama is not
    /// installed", which reads as "Mermaid needs Ollama". With a provider key
    /// present the message must be about naming a model, not about Ollama.
    #[test]
    fn missing_model_error_does_not_demand_ollama_when_a_provider_is_ready() {
        let config = Config::default();
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            let msg = no_model_configured_error(&config, false).to_string();
            assert!(msg.contains("anthropic"), "{msg}");
            assert!(msg.contains("mermaid --model anthropic/<model>"), "{msg}");
            assert!(msg.contains("[providers.anthropic]"), "{msg}");
            // Ollama may still be mentioned as the local option, but never as
            // a prerequisite for running Mermaid at all.
            assert!(!msg.contains("Ollama is not installed"), "{msg}");
        });
    }

    /// Run `f` with every built-in provider's key env var unset, so a key in
    /// the developer's own shell can't change what the message says.
    fn with_no_provider_keys<T>(f: impl FnOnce() -> T) -> T {
        let cleared: Vec<(&str, Option<&str>)> = [
            crate::providers::model::anthropic::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::LEGACY_API_KEY_ENV,
            crate::providers::model::meta::DEFAULT_API_KEY_ENV,
        ]
        .iter()
        .map(|env| (*env, None))
        .chain(
            mermaid_model::models::PROVIDER_REGISTRY
                .iter()
                .map(|profile| (profile.api_key_env, None)),
        )
        .collect();
        temp_env::with_vars(cleared, f)
    }

    /// With nothing configured at all, both routes are offered — the remote
    /// one first, since it needs no install.
    #[test]
    fn missing_model_error_offers_both_routes_when_nothing_is_configured() {
        with_no_provider_keys(|| {
            let msg = no_model_configured_error(&Config::default(), false).to_string();
            assert!(msg.contains("https://ollama.com/download"), "{msg}");
            // A keyring login would legitimately name a provider instead; only
            // assert the no-provider wording when there really is none.
            if !msg.contains("Remote providers ready") {
                assert!(msg.contains("ANTHROPIC_API_KEY"), "{msg}");
            }
        });
    }

    /// End-to-end through `resolve_model_id` itself: nothing pinned, no local
    /// model reachable, one configured provider — Mermaid starts on that
    /// provider instead of erroring out about Ollama.
    #[test]
    fn resolve_model_id_falls_back_to_a_configured_provider() {
        let mut config = config_with_provider_default("anthropic", "claude-x");
        // Point at a dead port with autostart off, so "no local model" holds
        // whether or not this machine has Ollama installed.
        config.ollama.host = "http://127.0.0.1".to_string();
        config.ollama.port = 1;
        config.ollama.auto_start = false;
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            let resolved = runtime
                .block_on(resolve_model_id(None, &config))
                .expect("a configured provider is enough to resolve a model");
            assert_eq!(resolved, "anthropic/claude-x");
        });
    }

    /// An installed-but-empty Ollama needs a pull, not another install.
    #[test]
    fn missing_model_error_says_pull_when_ollama_is_installed() {
        let msg = no_model_configured_error(&Config::default(), true).to_string();
        assert!(msg.contains("ollama pull qwen3:8b"), "{msg}");
        assert!(!msg.contains("https://ollama.com/download"), "{msg}");
    }
}
