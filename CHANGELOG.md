# Changelog

All notable changes to Mermaid CLI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **The two biggest `Result` families got real `# Errors` sections:
  `missing_errors_doc` 265 down to 153.** The only entry near the top of
  `clippy_pedantic.txt` with no machine-applicable fix — clippy cannot write
  the prose. 112 of the 265 sat in two files, both repetitive enough that a
  real contract exists to state rather than boilerplate to emit.

  `storage/repos.rs` (61) is a SQLite repository layer whose failure surface
  splits into families that say genuinely different things: a missing row is
  `Ok(None)` and not an error; one undecodable row fails a whole `list`; for
  a write, the reload is what produces the returned record; losing a `claim`
  race is `Ok(false)`; and `archive` runs its per-id updates outside a
  transaction, so a mid-loop failure leaves the earlier ids archived. The
  families were derived from what each body actually does — `.optional()`,
  `query_map`, `.context(...)`, transaction — not guessed from signatures.

  `runtime_client/client.rs` (51) is two parallel impls. `RuntimeClient`
  dispatches to the daemon and falls back to the local store, so what a caller
  needs to know is that a daemon which is simply not running is *not* an error,
  while one that rejects the request or answers with an unexpected shape is.

  The remaining 153 are a long tail across roughly forty files with no shared
  contract, needing per-call-site prose. Emitting 153 sentences of "Returns an
  error if the operation fails" would satisfy the lint and make the
  documentation worse, so they stay tracked.

- **The four remaining test-repo seeds could still collide on a commit hash.**
  The `worktree.rs` fix above covered one helper; `tests/subagent_worktree.rs`,
  `tests/subagent_worktree_scripted.rs`, `providers/tool/subagent.rs` and
  `providers/tool/workspace.rs` seeded theirs the old way. None had failed on
  it, which is the point: a shared hash does not cause a failure, it removes a
  failure's ability to clear on retry, so it stays invisible until some later
  assertion happens to be hash-sensitive and then reproduces forever.

- **`clippy::must_use_candidate` paid off entirely: 395 down to zero, and it
  took `return_self_not_must_use` from 30 to 6 with it.** Two keys' worth of
  movement from one machine-applicable pass — 80 keys become 79, and tracked
  debt falls from 2,877 occurrences to 2,458.

  Unlike the previous payoffs this one only adds: 395 lines inserted, none
  removed, no expression touched. The lint is worth honoring rather than
  dismissing as pedantic noise, because what it fires on is `pub fn` that
  returns a value and does nothing else — `is_busy`, `current_turn_id`,
  `input_total_tokens`, `TokenUsageTotals::new` — where calling and discarding
  the result is a bug by construction. `#[must_use]` makes the compiler say
  so. Builder methods returning `Self` are the overlap that carried
  `return_self_not_must_use` down as a side effect.

- **Two seeded test repos no longer share a base commit, which is what made
  the worktree flake stick.** `4b3133b` already found the real cause of that
  failure — `!listed.contains("a1")` matched the abbreviated commit hash in
  `git worktree list` output — and fixed it. What it documented but left in
  place was the second half: `init_project` wrote identical content under an
  identical `init` message, and git stamps commits to the second, so every
  test repo seeded inside one second got the *same* commit. Measured here,
  three repos in a row: `db33a16` all three times, a hash that happens to
  contain `a1`.

  That is why a 2.3% flake did not clear on retry. A nextest retry lands in
  the same second, rebuilds the same commit, and fails identically, so the
  failure reads as a race in `destroy` rather than as an unlucky hash. The
  seed message now carries the repo's unique directory name — the message and
  not the tree, because `tracked.txt` reads `"one\n"` in a dozen assertions.
  `two_project_repos_do_not_share_a_base_commit` pins it, and fails twice in a
  row against the old behavior, which is the property it exists to prevent.

  `porcelain()`'s doc comment has said "never the plain form" since that fix,
  while a call site four hundred lines below it stayed on
  `git worktree list` anyway. That one only counted lines, so it was harmless
  — but a comment is not a constraint. It is porcelain now, and
  `every_worktree_list_here_is_porcelain` reads this file's own source and
  fails with the offending line number if the plain form comes back.

- **The tracked `clippy::unwrap_used` count now measures shipped code, not the
  test suite.** It stood at 1,353 — 26% of all tracked debt and the single
  largest entry. Splitting the measurement showed what it was actually
  counting: against `--lib --bins`, which excludes `#[cfg(test)]` modules and
  the `tests/` targets, the same workspace reports **6**.

  So the number was never a risk measure. `unwrap()` in a test *is* the
  assertion — the panic is the failure being reported — and a metric that
  large and that test-shaped tracks how many tests exist. It had also begun
  charging new tests against a budget, which is a tax on the thing the repo
  wants more of.

  `allow-unwrap-in-tests = true` in `.clippy.toml` fixes it, alongside the
  `allow-dbg-in-tests` that was already there for the same reason. Nothing is
  suppressed that was ever enforced: `unwrap_used` is force-warned by the
  ratchet script alone and appears in no manifest's `[lints.clippy]`, so this
  changes what is counted, not what is allowed.

  Of the 6 in shipped code, 5 are `clap` `default_value_t` expansions where
  the `unwrap()` belongs to the derive macro. The sixth was real, in
  `ask_user_question`, and is gone: the remembered-answers path checked
  `all(|q| …is_some_and(…))` and then indexed `prefs[key.unwrap()]` three lines
  later, trusting a guard at a distance. It is one `remembered_answers` pass
  now, where the lookup that decides a question is settled is the lookup that
  builds its answer, so the two cannot disagree. That path had no direct test;
  it has four, including the no-`memory_key` case the old `unwrap()` was
  leaning on the guard to prevent.

- **`clippy::use_self` paid off entirely: 508 occurrences down to zero, and
  the key is gone from `clippy_pedantic.txt`.** The second-largest entry in the
  file, and the first tracked lint to be eliminated rather than reduced —
  81 keys become 80, and tracked debt falls from 4,717 occurrences to 4,209.

  Inside `impl Cmd`, `Cmd::CallModel` becomes `Self::CallModel`. The path
  already resolved to the impl's own type, so the rewrite cannot change what it
  refers to; the diff is 497 lines replaced by 497 lines, pure substitution
  with no structural change. The weight sits where the big enums are: `cmd.rs`
  124, `msg.rs` 80, `runtime_client/protocol.rs` 35.

- **`clippy::uninlined_format_args` paid off: 463 occurrences down to 6.** The
  third-largest entry in `clippy_pedantic.txt`, and the cheapest of the three
  above it — `format!("{}", x)` becomes `format!("{x}")`, machine-applicable,
  so `--fix` did the work. Tracked debt falls from 5,174 occurrences to 4,717.

  Nothing needed second-guessing here, which is what separates it from the
  `doc_markdown` payoff. That lint fires on any mixed-case word and wanted
  `` `OpenAI` `` fifty times, so a third of its suggestions were wrong. This
  one only fires when the argument is already a plain binding, so the rewrite
  cannot change what is printed. The review was for readability, and the
  collapse is a net win: 620 lines deleted against 481 added, because a
  multi-line `format!` folds back onto one line once its arguments move into
  the string.

  The 6 that remain are inside inline `#[cfg(unix)]` items under `src/`, which
  a `--fix` run on Windows never compiles and so never sees. The four
  `#![cfg(...)]` test files were reachable by the technique `AGENTS.md`
  documents — drop the gate, fix the target, restore the gate — and only
  `pty_exit.rs` had any (9). `daemon_integration.rs` and the two sandbox files
  compiled clean with zero hits.

- **The README is 75% shorter, and the `docs/` directory it linked to now
  exists.** It had grown to 53 KB / 752 lines — a third of that a single
  annotated config TOML, and most of the rest reference material a reader
  scrolls past to find the install command. A README's job is to say what this
  is, get you running, and point at the rest; it was doing all three badly by
  doing everything.

  The reference material moved rather than died: `docs/configuration.md` (the
  full schema, layering, project config, provider notes, web backends, API-key
  precedence), `docs/cli-reference.md` (every flag, the keyboard table, the
  slash-command list), `docs/sandbox.md`, `docs/runtime.md` (`mermaidd`,
  logging, diagnostics), `docs/plugins.md` (skills, hooks, bundles), and
  `docs/development.md` (the pre-PR gate, CI matrix, snapshot suites). The
  README keeps what belongs there: features, install, first ten minutes, the
  common commands, the tool table, safety, and the provider env-var table.

  `docs/` is new because it did not exist. The README had linked to
  `docs/architecture.md`, `docs/adding_tools.md`, `docs/adding_providers.md`
  and `docs/replay_debugging.md` since before this change, and all four were
  404s — a promise of a contributor guide that was never there. Those dead
  links are gone; the recipes they named are still unwritten and are not
  claimed to exist.

  `tests/readme_drift.rs` follows the content. Its config-schema anchors
  (`external_writes`, `system_installs` must stay documented) now assert
  against `docs/configuration.md`, where that schema lives, while the README's
  smaller starter config keeps its own must-parse check. A third guard,
  `readme_doc_links_resolve`, walks every `](docs/...)` link in the README and
  fails if the file is absent — the exact failure that shipped four dead links
  and survived, because nothing looked.

- **A message's "Today"/"Yesterday" label now follows the injected clock, not
  the machine's.** `format_relative_timestamp` called `Local::now()` itself to
  pick between "Today at ...", "Yesterday at ...", and an absolute date. That
  choice *is* the output, so the rendered string was a function of the wall
  clock rather than of its arguments — the last clock read under the render
  path, left behind when `ChatWidget::today` made the frame-memo key pure.
  `mermaid-model` sits outside the layering guard's scope, so nothing flagged
  it.

  It takes `today: NaiveDate` now, threaded from `state.now` by the same route
  `ChatWidget::today` already used. Key and label agree by construction instead
  of by both calling `Local::now()` a microsecond apart, and `--replay` labels a
  recorded transcript against the recorded date.

  Twenty snapshots change: scene messages are stamped at the fixture clock, so
  they render "Today at 3:04am" where they used to render "January 2nd, 2026 at
  3:04am". That is what a user with that clock sees; the old string was an
  artifact of the suite's fixture date being in the past while the formatter
  read the real one. The fixture comment claiming the past date is what forces
  the stable absolute-date branch is rewritten — the branch is now pinned by
  construction, and the date being in the past is merely conventional.

  `fixture_clock_reads_the_pinned_wall_clock` deliberately does *not* pass the
  fixture's own date. It exists to pin the rendered date and time, and the
  day-relative branches print neither; passing `fixed_now().date_naive()` would
  render "Today at 3:04am", and a `fixed_now()` that drifted across midnight
  would drag `today` with it and still say "Today". It passes a distant date so
  the absolute branch keeps the assertion diagnostic.

  The unit test gains the two branches it could not previously reach: with the
  clock read inlined, "Yesterday" and the absolute date were unreachable from a
  test on any given day.

- **`clippy::doc_markdown` paid off, and `.clippy.toml` now tells it which
  words are prose.** It was the single largest entry in `clippy_pedantic.txt`
  at 345 occurrences — a quarter of the tracked debt, and the cheapest quarter,
  since the lint's suggestion is machine-applicable.

  Running `--fix` blind would have been a mistake. The lint fires on any
  mixed-case word, so of its first 313 suggestions roughly a hundred were
  wrong: it wanted `` `OpenAI` `` (50 times), `` `OpenRouter` `` (26),
  `` `SearXNG` `` (23), `` `SQLite` ``, `` `PyPI` ``, `` `ConPTY` ``, and
  `` `POSTed` ``. Backticks mean "this is an identifier"; a vendor's name in a
  sentence about the vendor is not one, and marking it up as code is a false
  claim in 100 places.

  So the nineteen genuine prose words are declared in `doc-valid-idents` (with
  `".."`, extending clippy's built-in list rather than replacing it) and the
  remaining 183 suggestions were applied. Those are real: type names, enum
  variants, config keys, `find` flags, and the SDDL codes `GA`/`SY`, which
  *are* literal tokens and do belong in backticks. Two more were fixed by hand
  rather than by the tool — one where it backticked `len()` out of the middle
  of an expression, and one unbalanced-backtick warning it could not fix at all
  (a doc comment mentioning ```` ```json ```` fences).

  `.clippy.toml`'s stated invariant — every key configures a lint that is
  actually enabled — is widened rather than quietly broken: a key may now also
  configure a lint named in `check_clippy_ratchet.py`'s `LINTS`, which is
  tracked on every push to `main` without blocking a PR. `doc_markdown` stays
  non-blocking; nothing here promotes it into the `[lints.clippy]` table.

  `clippy_pedantic.txt` goes from 82 keys / 5519 occurrences to 81 / 5174 —
  the `doc_markdown` key disappears entirely. The number is taken from the CI
  log of a `workflow_dispatch` run on the branch, since the baseline is a Linux
  measurement and this was authored on Windows; the `Lint Debt` job does not
  run on pull requests, and `workflow_dispatch` is what satisfies its `if`.

  One knock-on: backticking `resource_link` pushed `ContentBlock`'s doc comment
  in `src/mcp/client.rs` two characters past the `too_long_first_doc_paragraph`
  limit, which would have moved that key from 203 to 204. Split into a summary
  line and a body rather than recorded — a baseline that only shrinks should
  not absorb a rise just because it arrived in the same diff as a larger fall.
- **The layering baseline is empty: 3 keys / 6 occurrences to 0 / 0.** The
  guard landed with its recorded debt intact; this pays the last of it, so
  `layering.txt` is now an assertion rather than an allowance.

  Three reads, all of them injected instead of deleted, because each value is
  genuinely needed by the pure code that consumed it:

  `env::temp_dir()` in `State::new` — the reducer joins it into pasted-image
  scratch paths, which is pure path construction over an impure input. It is
  now a `State::new` parameter, passed by the shell, alongside `cwd` and `now`.
  It sits last in the argument list rather than beside `cwd`: two adjacent
  `PathBuf` parameters are silently swappable and nothing in the type system
  would catch it.

  `HOSTNAME`/`HOST` and `USER`/`USERNAME` in `RenderCache::default` — the
  status bar's `user@host`. `RenderCache::new` now takes them; `app::run`
  reads the environment. The `Default` impl is gone rather than kept beside
  the new constructor.

  `chrono::Local::now()` in the chat widget's frame-memo key — a user
  timestamp renders as "Today"/"Yesterday"/an absolute date, so the date is a
  real render input. It arrives as `ChatWidget::today`, derived from the
  injected `state.now` by the same route `blink_on` already took.

  That last one purifies the *key*, not yet the label: the string itself comes
  from `mermaid_model::utils::format_relative_timestamp`, which still calls
  `Local::now()` internally. `mermaid-model` is outside the layering guard's
  scope, so this is the honest edge of what "0 occurrences" claims — the render
  tree no longer reads the clock, and one crate below it still does. Live, the
  two agree, because `state.now` is re-seeded from the wall clock every loop
  iteration. Threading the date through the formatter as well would change what
  the snapshot suite renders (its fixture date is deliberately in the past so
  every user timestamp falls through to the absolute-date branch), so it is a
  separate change.

  The snapshot and bench rigs previously pinned `hostname`/`username` by
  assigning over the environment-derived values after construction. They now
  pass them, and the three render tests that took whatever machine they ran on
  pass pinned literals too.

- **The purity guard now checks dependency direction, not just I/O tokens.**
  `check_domain_purity.py` scanned `src/domain` for seven strings —
  `std::fs`, `tokio::`, `reqwest`, and friends. `use crate::app::Config`
  contains none of them, so the guard reported OK while the "pure" MVU core
  accumulated 34 production edges into `app`, `session`, `providers`, and
  `render`. Two of those are cycles: `app::CompactionConfig::policy()` returns
  a `domain::CompactionPolicy`, and `app::stamp_session_provenance` takes
  `&mut domain::State` — which `state.rs` opens by declaring that nothing
  outside `update()` may hold one.

  It also read less of the tree than it appeared to. It truncated each file at
  the first `#[cfg(test)]`, so it scanned 43% of `reducer.rs`; and its `ROOT`
  was `src/domain` alone, so `src/render` was never scanned at all despite
  AGENTS.md claiming CI enforced its purity. `src/render` was not pure when
  the guard first scanned it: `chrono::Local::now()` sat in a render cache key
  and `std::env::var` fed the status bar. (Both are gone — see the layering
  baseline entry above, which lands in this same release.)

  `.github/scripts/check_layering.py` replaces it. It covers `src/domain`,
  `src/render`, and `src/prompts.rs`; enforces a declared layer table so an
  upward import fails with the rationale attached; blanks each `#[cfg(test)]`
  item by brace matching instead of truncating; resolves
  `#[cfg(test)] mod foo;` to whole test-only files; and lexes comments, string
  literals, and nesting-aware block comments out before matching. The
  forbidden-token list gains `std::env`, `std::io`, `std::thread`,
  `Command::new`, `rusqlite`, `.await`, `Local::now`/`Utc::now`, and `unsafe`.

  Pre-existing debt is recorded in `.github/baselines/layering.txt` — 16 keys,
  47 occurrences — via a shared ratchet (`.github/scripts/ratchet.py`) that
  fails CI when the count *rises* and equally when it *falls without the file
  being updated*. A baseline that could only be appended to is a place debt
  goes to be forgotten; requiring it to be edited down puts the number in the
  diff.

- **`just check` runs the source guards.** AGENTS.md called it "the exact
  pre-PR gate (also what CI runs)" while CI ran two guards it did not.

- **The lints `.clippy.toml` had been configuring are now enabled.** AGENTS.md
  said "clippy caps functions at 100 lines". It did not: `too_many_lines` is a
  *pedantic* lint, no manifest carried a `[lints]` table, and CI ran stock
  `clippy::all`. The threshold configured a lint that never ran, which is how
  `update_step` reached 774 lines — and how a `#[allow(clippy::too_many_lines)]`
  came to sit in `exec.rs` suppressing nothing.

  All three manifests now carry an identical `[lints.clippy]` table denying
  `too_many_lines`, `excessive_nesting`, `dbg_macro`, `todo`, `unimplemented`,
  and `mem_forget`. The table is duplicated rather than inherited via
  `[workspace.lints]`: the `isolated-crate-build` job copies `crates/.` to a
  directory with no workspace root, where `lints.workspace = true` is a hard
  manifest-parse error. `tests/lint_policy_drift.rs` guards the duplication and
  pins that the threshold and the lint stay enabled together.

  The 59 functions already over the cap carry
  `#[expect(clippy::too_many_lines, reason = ...)]`, counted in
  `.github/baselines/expect_budget.txt` (45 keys, 69 occurrences) which only
  shrinks. Every pre-existing `#[allow(clippy::…)]` became `#[expect]` so
  `unfulfilled_lint_expectations` reports a suppression that stops being
  necessary — which immediately found four suppressing nothing
  (`large_enum_variant` on `Msg`, `too_many_arguments` on `block_for_approval`
  and `hard_break_styled_word`, `too_many_lines` on
  `finish_foreground_command`). All four removed.

  Seven now-inert keys were deleted from `.clippy.toml` rather than left
  looking load-bearing, under a stated invariant: every key must configure a
  lint that is actually enabled.

- **`update_step` enforces its own no-wildcard rule.** AGENTS.md promised the
  reducer has "no wildcard `_ =>` arms that hide new `Msg`s"; nothing checked
  it, and the top-level `match msg` merely happened to stay exhaustive. It now
  carries `#[deny(clippy::wildcard_enum_match_arm,
  clippy::match_wildcard_for_single_variants)]`. Both, because the first only
  fires when `_` covers two or more remaining variants — a `_` added beside a
  single unhandled `Msg` would slip past it. Function-scoped, so the 96
  legitimate `_ =>` arms on `KeyCode` and friends elsewhere are unaffected.

- **The re-export shims are gone.** AGENTS.md bans back-compat shims; the two
  largest covered ~1,046 call sites against 2 that named a crate directly. That
  ratio is now 0 to 1,059. `crate::models::` read as a local module, so nothing
  in a diff showed `src/domain` reaching across a crate boundary — the exact
  property the crate split exists to make visible.

  Deleted: `src/lib.rs`'s re-exports (including
  `pub use app::{Config, load_config, persist_last_model}`, which had zero
  consumers), `pub use mermaid_runtime::*`, the three 100%-shim files
  `src/domain/{action,ids,question}.rs`, the embedded `tool_run` re-export in
  `src/domain/runtime.rs`, `src/effect`'s re-export of `models::retry` (which
  no consumer could reach), and `crates/mermaid-model/src/utils/redact.rs`.
  Plus 34 dead `pub use` names and 4 unused dependencies (`sysinfo`,
  `tokio-stream`, `tracing-subscriber`, `tempfile`).

  `src/runtime/` is now `src/runtime_client/`. It is the daemon client and sits
  *above* `mermaid-runtime`; sharing a name with the crate it wraps is what made
  `crate::runtime::Foo` ambiguous.

- **A crate-root `pub use` must now have a consumer.**
  `.github/scripts/check_exports.py` fails on a name re-exported from
  `src/lib.rs` or `crates/*/src/lib.rs` that nothing in the workspace names.
  This is the one clause of the "no back-compat shims" rule the compiler cannot
  help with: a `pub` item is reachable by definition, so `-D warnings` never
  sees it. The measured cost of not having the check was 34 such names — all
  twelve `*Repo` types in `mermaid-runtime`'s root, six `OUTCOME_*` constants,
  and a `redact` forward that had drifted asymmetrically, so
  `crate::utils::redact_json` resolved while `redact_json_text` did not.

  `cargo-public-api` and `cargo-semver-checks` were both considered and
  declined: they guard an API these crates explicitly do not promise, and
  `semver-checks` conflicts outright with the delete-cleanly rule.

- **`cargo deny` replaces `cargo audit`; `cargo machete` joins it.** Same
  RustSec database, and `deny.toml` sets `unmaintained = "workspace"`, which
  preserves the judgement the old step encoded by declining `--deny warnings`.
  Added are the three questions `audit` cannot ask: license compatibility
  (Mermaid ships static binaries under `MIT OR Apache-2.0` and nothing checked
  for a transitive copyleft dep), bans (`openssl-sys` must never reappear in a
  deliberately rustls-only build, and `wildcards = "deny"` stops `foo = "*"`),
  and source provenance.

  `cargo machete` immediately found five dependencies the root manifest
  declared and never imported: `bytes`, `regex`, `url`, `nucleo-matcher` — the
  last three left behind when their code moved down to `mermaid-domain` — plus
  `keyring` and its companion `dbus-secret-service`, whose every call site is
  in `mermaid-model`. Workspace feature unification is why `cargo build` never
  noticed. All six removed; `cargo tree --target x86_64-unknown-linux-gnu`
  confirms `dbus-secret-service/vendored` and all four keyring features still
  resolve, now sourced from `mermaid-model` alone.

- **Pedantic and nursery lint debt is tracked.** A `clippy-ratchet` job records
  `pedantic`, `nursery`, `unwrap_used`, `panic`, `wildcard_enum_match_arm`,
  `string_slice`, `trivially_copy_pass_by_ref` and `many_single_char_names`
  against `.github/baselines/clippy_pedantic.txt` — 85 lints, 5,378
  occurrences. It does not run on pull requests: enabling these lints changes
  clippy's fingerprint, so the job cannot share the blocking `Clippy` job's
  cache and rebuilds the workspace, and the answer does not change PR-to-PR.
  The survey uses `--force-warn` rather than `-W`, because a `deny` in
  `mermaid-runtime` aborts the build before the crates above it are linted —
  which is how the first `too_many_lines` count of this workspace came back as
  3 when the real number was 59.

### Fixed

- **A worktree test failed roughly once in 43 runs on a commit hash, not on a
  bug.** `destroy_leaves_no_checkout_and_no_git_bookkeeping` asserted
  `!listed.contains("a1")` over the output of `git worktree list` — which
  prints `<path> <abbrev-hash> [<branch>]`, so the substring test was also
  testing the commit hash. A 7-hex-char abbreviation contains `a1` about 2.3%
  of the time (measured at 7 of 300; 6/256 by construction), and the test then
  failed against bookkeeping that had in fact been pruned correctly.

  It did not self-clear on retry either, which is what made it look like a
  race. `init_project` commits identical content under an identical message
  and git stamps commits to the second, so a nextest retry landing in the same
  second rebuilds the *same* commit and fails identically — verified: two
  inits one second apart produced different hashes, two within one second
  produced the same one. Only a job re-run minutes later drew a new hash. Two
  CI failures on 2026-08-08, on branches that changed doc comments and
  timestamp formatting respectively, were this and not the worktree code.

  The query is now `git worktree list --porcelain`, whose `worktree <path>`
  lines carry no hash, matched against the checkout's own directory name
  (`a1-<pid>-<seq>`) rather than the bare agent id. A new
  `a_live_checkout_is_listed_in_the_bookkeeping` is the matched positive
  control: without it the negative assertion would keep passing even if the
  query silently matched nothing. Both were checked against an injected fault
  — a `destroy` that deletes the checkout but skips git's bookkeeping — which
  the tightened assertion catches and the control survives.

  `AgentWorktree::destroy` is unchanged. It was never the problem.

- **Windows: hardening the data directory locked Mermaid out of its own
  runtime database.** The one-shot `icacls <dir> /inheritance:r /grant:r
  <user>:(OI)(CI)F /T` did exactly what it was told, and what it was told was
  wrong: `(OI)(CI)` are *inheritance* flags describing what the children of a
  container inherit, and `/T` applied that ACE string verbatim to every
  existing file underneath. On a leaf file those flags grant nothing, so the
  inherited ACE was stripped and nothing replaced it. Measured on a fresh
  directory, every file came out with an **empty DACL** and every directory was
  correct.

  SQLite then returned SQLITE_CANTOPEN (14) on every open, so `/runtime tasks`,
  approvals, checkpoints, process listing and the `mermaidd` daemon were all
  unavailable — and a `.acl-hardened` sentinel, written on `icacls` exit 0,
  meant it never ran again. No data was lost (an owner always keeps
  `WRITE_DAC`) but nothing took the access back.

  Hardening now targets the **directory only** and lets Windows propagate to
  children, which is what keeps subdirectories' `(OI)(CI)` flags intact so
  files created later inherit correctly too. The sentinel is written only after
  the database is confirmed openable, not on `icacls` exit 0 — those are
  different claims, and the gap between them is the whole bug. Machines already
  in this state repair themselves: a SQLITE_CANTOPEN on a file that exists
  restores owner access once, logs that it did, and retries.

- **A runtime database older than schema v2 could never be upgraded.** The F75
  covering index `idx_tasks_status_owner ON tasks(status, owner_kind)` was
  created in the idempotent baseline, which runs *before* the `ensure_column`
  that adds `owner_kind`. On a v0 or v1 database `CREATE TABLE IF NOT EXISTS
  tasks` is a no-op against a table that has no such column, so the index
  failed with `no such column: owner_kind`. `IF NOT EXISTS` does not help — it
  guards the index *name*, and SQLite still parses the column list.

  The failure landed inside the migration transaction, so it rolled back and
  `user_version` was never stamped, and the next open failed identically.
  Permanently: tasks, approvals, checkpoints, process listing and the daemon
  were all unreachable with no way forward.

  The index now sits after its `ensure_column`, exactly as
  `idx_checkpoints_session` already did. Verified on a real 2.7 MB v1 database,
  which now migrates to v6 with its data intact.

  It survived because the two existing migration tests started at v2 and v5 —
  the versions that were convenient to construct, and both of which already
  have the column. `every_supported_older_version_upgrades_to_current` now
  covers the whole accepted range and strips each database back to the *shape*
  that version really had rather than only rolling `user_version` back.

- **`cargo doc` accepted broken intra-doc links.** The CI step ran without
  `-D warnings`, so rustdoc printed unresolved links and exited 0. Twenty had
  accumulated, including `TaskBroker::note_tokens` — a method renamed to
  `add_tokens` with three doc references left pointing at the old name — plus
  `<provider>/<model>` placeholders that rustdoc parsed as unclosed HTML tags.
  All fixed, and the gate is now `RUSTDOCFLAGS: -D warnings`.

- **`mermaid self-test` said the runtime store failed to open without saying
  why.** The error was rendered with `err.to_string()`, which prints an
  `anyhow::Error`'s outermost context and drops the chain — and the outermost
  context is always the same `failed to open runtime DB <path>`. `{err:#}`
  now carries the cause, which is the sentence that distinguishes a locked
  file from a permissions denial from a schema this build will not migrate.

## [0.21.1] - 2026-08-07

### Fixed

- **`mermaid-model` could not be published, which left v0.21.0 half-released.**
  The crate uses `keyring`, whose Linux `sync-secret-service` backend needs
  libdbus. The root crate has carried a Linux-only dep on
  `dbus-secret-service` with its `vendored` feature for exactly that reason —
  static libdbus, no system headers — and the new crate's manifest never got a
  copy of it.

  Nothing caught it, because `cargo build --workspace` unifies features across
  members: the root turned `vendored` on for everybody, so the workspace built
  green while `mermaid-model`'s own manifest was incomplete. `cargo publish`
  builds a crate ALONE, and that is where it surfaced — after `mermaid-runtime`
  0.21.0 had already gone to crates.io, and after the GitHub release, the
  binaries, Homebrew, Scoop and WinGet had all shipped. The local
  `--dry-run` could not have caught it either: on Windows keyring uses
  `windows-native` and never touches dbus.

  Two changes so it cannot repeat. A `Crates build standalone (Linux)` job
  packages each crate and builds the extracted tarball on every PR, which is
  what publish does and what feature unification hides. And the publish step
  now skips versions already on crates.io instead of aborting on them, so a
  partial failure can be resumed rather than stranding the release.

## [0.21.0] - 2026-08-07

### Added

- **Worktree isolation for subagents.** A subagent has always run in the
  parent's directory, which is right for one child and wrong for fan-out:
  `MAX_INFLIGHT` is 10, and ten children editing one working copy produce a
  tree that matches no single agent's intent. `path_lock` already stopped two
  writers to the *same path* from losing each other's bytes, but nothing
  stopped child A's build from compiling child B's half-finished edit, and
  nothing stopped two individually-correct changes from being jointly
  incoherent. Set `isolation = "worktree"` on an agent type, or pass the
  `isolation` tool argument per call, and the child gets a private git
  checkout under the data dir instead.

  The checkout is seeded with the project's uncommitted state — tracked
  modifications via a binary diff, plus untracked-but-not-ignored files —
  and that state is committed as the child's *base*. Starting from `HEAD`
  would have been simpler and would have hidden the user's work in progress
  from every isolated child, which is the case that matters. Ignored files
  are deliberately left behind: that is what keeps a checkout cheap, and it
  is also why an isolated child that builds pays a cold cache.

  On success the child's work is diffed against its base, checkpointed, and
  applied to the project as one patch, serialized on the project root against
  other children. The apply is dry-run first and never `--3way`: a patch that
  does not apply leaves the project **untouched**, saves itself next to the
  worktree, and is reported as a failure even though the child itself
  succeeded — a child whose work did not land must not read as success, or
  the parent builds on changes that are not there. Two agents editing the
  same lines now produce a named conflict instead of a silent interleave.

  A merge re-anchors the base, so continuing an isolated child merges only
  its new work rather than replaying what already landed. A timed-out or
  errored child is not merged at all — half a change is worse than none —
  and its checkout is kept, with its location in the report, for the
  continuation that a timeout invites. Cancelled children discard theirs.
  Isolation outside a git repository fails the spawn rather than quietly
  falling back to shared, which would put a fan-out that asked for isolation
  back into the collisions it asked to avoid with nothing in the transcript
  saying so. Isolated children are told they are isolated, so they neither
  report edits the user cannot see nor go hunting for the "real" project.

- **`mermaid_runtime::worktree`** owns that lifecycle (create, seed, merge,
  destroy) and `gc_orphaned_worktrees` sweeps checkouts stranded by a crash —
  agent ids are per-session, so nothing reclaims one by name after a restart.
  The daemon runs it at startup alongside the checkpoint GC.

  Paths the worktree hands out are anchored on a **canonicalized** project
  root, not on git's `--show-toplevel` answer. Those paths are the merge's
  write-lock keys, and the file tools lock on canonicalized paths — two
  spellings of one file are two keys, so a merge and a concurrent
  `write_file` would not have excluded each other at all. Windows CI found
  it: `%TEMP%` hands out an 8.3 short name where git reports the long one.
  A symlinked route to the same directory would have done the same on any
  platform.

  Checkout directories carry a process id and a counter, not just the agent
  id. Agent ids restart at `a1` per spawner, so two Mermaid processes in one
  repo both wanted `.../a1`; git resolved that by inventing `a11` and `a12`,
  and the two then fought over each other's bookkeeping — the loser died on
  `index.lock: File exists`. Found by the live fan-out test below, not by
  review. Once the names are distinct, git serializes its own worktree
  bookkeeping and needs no lock of ours; both `concurrent_creates_on_one_repo`
  and `creating_and_destroying_at_once` hold that, and were checked by
  mutation (a repo-level lock was written first, then removed when neither
  test could be made to fail without it).

  Mermaid's own session state is excluded from a child's patch. The runtime
  writes the child's transcript to `<workdir>/.mermaid/conversations/` as it
  runs; inside a checkout, `git add -A` swept that into the diff and merged
  it into the user's repository — files no agent wrote and nobody asked for.
  The exclusion is deliberately narrow: the rest of `.mermaid/` (`config.toml`,
  `memory/`) is the user's, so a child asked to edit those still can. Found by
  the scripted suite below, on the one test that asserted a child which wrote
  nothing changes nothing.

- **The merge's own failure paths are forced, not just reasoned about.** Two
  branches in `Workspace::merge` existed for cases nothing exercised. A
  checkout that vanishes mid-flight now reports that reading its changes
  failed rather than the far more dangerous "changed no files", which would
  tell the parent its child did nothing rather than that the work was lost.
  And a merge whose pre-apply checkpoint cannot be written applies nothing —
  merging with no restore point behind it would leave the change
  unrecoverable. The checkpoint case needs an unreadable file to force, so it
  is `#[cfg(unix)]`, matching how the repo already scopes mode-dependent
  tests. `git` spawn failures now name the working directory, since a missing
  directory is a likelier cause than a missing git and the old message
  guessed wrong.

- **A stub provider for tests** (`tests/harness/stub_model.rs`) and
  `ProviderFactory::with_seeded_providers`, the seam it plugs into. A
  `ScriptedModel` replays a queue of turns — tool calls, then a final message
  — so a test drives the real reducer, effect runner, tool registry, safety
  gate, and subagent spawner without a network. Nothing in production
  dispatch knows it exists; the stub lives in `tests/` and is injected, so no
  model id can reach it. Running off the end of a script panics rather than
  returning empty, because that means the loop took a path the test did not
  describe.

  `tests/subagent_worktree_scripted.rs` uses it for the cases a live model
  will not produce on demand: two children editing the same line so exactly
  one conflicts, four disjoint children merging concurrently, what the child
  was actually told about being isolated, and the empty-merge case that
  caught the leak above. Offline, no key, on every `cargo nextest run`.

- **Deterministic coverage for the Auto-mode safety classifier**
  (`tests/auto_classifier_stubbed.rs`). In `auto` mode an LLM decides whether
  a borderline action runs without asking, which makes
  `ModelAutoClassifier::vet` a security boundary — and its tests covered only
  the pure helpers (`parse_verdict`, `looks_like_injection`), never the path
  from request to verdict. The two properties a live model cannot demonstrate
  on demand are now pinned: it **fails closed** (a provider error, a stall
  plus cancellation, or an unparseable reply escalates rather than allows),
  and it **does not leak** (the stub records what the process actually sent,
  so a secret in the vetted action is asserted absent from the wire rather
  than assumed redacted). Both were checked by mutation — making the error
  arm return `allow`, and stripping the redaction calls, each fails exactly
  one test.

- **Deterministic coverage for compaction's failure modes**
  (`tests/compaction_stubbed.rs`). Compaction is the one operation that
  deliberately destroys conversation state, and it is a *two-call* operation
  — draft, then review — so nothing could reach its failure paths before
  without a live model failing on cue. Now pinned: a failed draft call
  reports an error and touches nothing; a failed or malformed **review** call
  still lands the valid draft rather than costing the user their context over
  a second call that was only ever a quality improvement, and records the
  reason in `review_error`; a draft and review that are both structurally
  invalid fail rather than replacing real history with prose; and "nothing to
  compact" stays an `Info` note that spends no model call, so compaction
  errors remain worth reading. Mutation-checked: making a review failure sink
  the whole compaction fails exactly one test.

- **Deterministic coverage for memory consolidation**
  (`tests/memory_consolidation_stubbed.rs`). `/memory consolidate` hands the
  model the whole corpus and acts on a JSON prune list by **deleting files**;
  `parse_prune_plan` was unit-tested but acting on the result was not. Now
  pinned: unparseable output, a hallucinated plan naming ids that do not
  exist, an empty plan, and a dead provider each delete nothing and say so;
  a valid plan prunes exactly what it named and nothing else; and a corpus
  too small to compare short-circuits without shipping itself to a model.
  Fixture ids are process-unique because `memory_roots` spans the global
  scope — a prune naming a real memory would delete it.

- **Deterministic coverage for the provider-facing half of the agent loop**
  (`tests/agent_loop_stubbed.rs`). The reducer's own logic is already covered
  purely, so this is only what needs a provider in the loop: an opaque
  continuation blob surviving from the provider to the reducer and back onto
  the next outgoing request (break it and nothing errors — the model just
  quietly loses its reasoning); one turn's parallel tool calls all reaching
  the reducer with their arguments intact; a provider error keeping its own
  reason instead of flattening to "something went wrong"; and Ctrl+C aborting
  an **in-flight model call**, which `tests/effect_cancel.rs` did not cover —
  it cancels a running tool, not a running generation.

- **Deterministic coverage for the subagent lifecycle**
  (`tests/subagent_lifecycle_stubbed.rs`). `build_child_registry` had a unit
  test for which tools it *builds*; this covers which tools the child's model
  was *told about*, since the request is assembled well downstream — an
  `explore` child advertised `write_file` would call it. Also: a continuation
  really carries the first drive's prompt and answer into the second, and
  backgrounding a child releases the turn at once while its report still
  arrives out of band.

- **A subagent's parallel tool calls are executed, not just counted.** The
  agent-loop suite checks a turn's calls reach the reducer against an empty
  registry; a child now runs three real `write_file` calls from one turn and
  is shown to receive all three results before its next turn. Losing one
  would hang the child until its timeout.

- **A live end-to-end suite for isolation** (`tests/subagent_worktree.rs`,
  `#[ignore]`d, **run by hand**). The unit tests cover the mechanics with the
  drive stubbed or failed on purpose; only a real child shows that a model
  handed an isolated workspace actually writes into the checkout and that its
  work then lands — that is what caught the agent-id collision below. Three
  tests: a single child's edit landing, a shared child still writing straight
  through, and three children fanning out concurrently without collision.
  Runs on `meta/muse-spark-1.2-contributor`.

  Deliberately **not** wired into CI. Each case is a paid API call on every
  push, and without a repository secret every case skips while printing
  `ok. 3 passed` in 0.00s — coverage-shaped noise for real money. The default
  suite already compiles the file on all three platforms, so a refactor that
  breaks these still fails CI; only the live execution is manual.

### Changed

- **One hardened path for every `git` invocation** (`mermaid_runtime::git`).
  `checkpoint.rs` and `plugin.rs` each carried their own `Command::new("git")`
  wrapper, and they disagreed: checkpoints disabled hooks but not `ext::`
  transports, and only the plugin path blocked credential prompts. The shared
  builder applies all of it everywhere — no repo-provided hooks, no external
  transports, no terminal prompts, a fixed committer identity — and adds
  stdin support so `git apply` takes a patch without a temp file. Checkpoint
  shadow-git now goes through it, which is a net tightening of the checkpoint
  path.

- **The render snapshot suite runs on Windows.** It was `#[cfg(all(test, unix))]`,
  so `just check` on Windows silently skipped every pinned frame — the platform
  with the least coverage was the one where a contributor was most likely to
  believe they were covered, and three v0.20.0 bugs that only a rendered frame
  could catch were invisible there. Measured rather than assumed: of the two
  reasons in the cfg comment, the unix-path one was already moot (the scenes use
  a literal cwd, which prints identically on Windows), and the timezone one was
  real but fixed the wrong way. `TZ=UTC` around each scene cannot work on
  Windows, where chrono reads the system zone and ignores `TZ` — and it was not
  working on unix either: chrono resolves the local zone once per process and
  caches it, so mutating `TZ` mid-test changes nothing. The snapshots matched
  because CI runs in UTC. So the fixture clock now names a fixed *local wall
  clock* instead of a fixed instant: the frame formats it in local time, so a
  fixed instant renders a different stamp in every zone while a fixed wall clock
  renders the same one everywhere. The suite is timezone-INDEPENDENT rather than
  timezone-pinned, which is what lets one set of `.snap` files serve every
  platform — they are unchanged by this, and a UTC-5 Windows box now matches
  frames generated under UTC. `fixture_clock_reads_the_pinned_wall_clock` guards
  it, verified by mutation: with the old fixed-instant clock restored and
  `TZ=Pacific/Kiritimati` set, it fails alongside every scene carrying a user
  message.

- **CI: the security job stopped building its own tooling, and pty snapshots
  settle on observation instead of a stopwatch.** Following #301's finding that
  ~90% of a leg is compilation, the remaining time was measured rather than
  guessed at. Two things were not compilation.

  `rustsec/audit-check` compiled cargo-audit from source on every run — **221s
  in a single step**, on a job whose real work is reading `Cargo.lock` against
  an advisory database, and 94% as long as the Windows leg that gates the
  merge. It now installs a prebuilt binary through the same
  `taiki-e/install-action` already used for nextest and runs `cargo audit`
  directly: same tool, same RustSec database, same strictness (plain `cargo
  audit`, not `--deny warnings`, so the gate is exactly as tight as it was).

  The pty harness slept a flat 500ms + 200ms before every golden-frame
  snapshot. That is two bets in one: too slow when the app has already
  repainted, and — the half that actually matters — a fixed wager that a loaded
  runner finishes painting inside 700ms. `settled_frame` now polls until the
  grid stops changing for three consecutive reads, capped at that same 700ms,
  so the slow path is no less settled than before and the common one is ~150ms.
  `safety_mode_footers` went 4.98s -> 1.62s, `assistant_markdown_frame` 1.64s ->
  0.41s, the three pty suites together 5.37s -> 4.07s. Eight consecutive runs
  produced no snapshot mismatch and no `.snap.new`.

  Two candidates were measured and **rejected**, which is the more useful half
  of the result. Linking with `rust-lld` instead of `link.exe`: 35.0s vs 35.7s
  on the exact rebuild CI pays — the compile is codegen-bound, not link-bound.
  Merging the 19 integration-test binaries into one: a single test binary
  rebuilds in 1.9s, not the ~8.3s a contended `--timings` report implies, so
  the whole idea is worth a few seconds and not the disruption to
  `--test <name>` selectors. Dependencies were confirmed fully cached — only
  `mermaid-cli` and `mermaid-runtime` compile on a CI leg.

- **`mermaid-model`: the model layer is now a crate, and the compiler enforces
  it.** Five dependency edges pointed the wrong way — the wire adapters reached
  up into the application. `models` read `app::Config`, borrowed `prompts`'
  system prompt, borrowed `domain::ActionDisplay`, called the retry middleware
  up in `effect`, and invoked `ollama::ensure_running` to spawn a server. Each
  was invisible in review because `crate::` makes every module equidistant.

  All five are gone, and the new crate boundary makes them unrepresentable:
  adding `use crate::app::Config` back to `models/config.rs` is now an
  `unresolved import`, not a code-review conversation.

  The one that mattered was Ollama autostart. A wire adapter that can spawn a
  process has no way to tell a caller "this path must not mutate anything" —
  which is exactly how a connection-refused retry lived unnoticed inside a
  read-only listing path. Recovery is now an injected capability
  (`LocalServerRecovery`), supplied by the layer that owns the process. The
  enumeration verbs are read-only *by construction* rather than by a `bool`
  each one has to remember to pass.

  `mermaid-cli` re-exports every moved module under its historical path — the
  same shim `crate::runtime` already used for `mermaid-runtime` — so **not one
  of the ~645 `crate::models::…` / `crate::utils::…` / `crate::constants::…`
  call sites changed.**

  Along the way, two things fell out that were bugs rather than layering:

  - `ModelConfig::from_app_config` was **dead code duplicating a live rule**.
    Nothing had ever called it, while `State::new` carried a byte-identical
    copy of the same per-model reasoning resolution — and its three tests were
    the only coverage that rule had anywhere, aimed at the copy that didn't
    run. The duplicate is deleted and the tests now assert against
    `State::new`, so the rule that actually executes is the one under test.
  - `ModelConfig::default()` cloned a multi-KB system prompt on **every**
    construction, including every `..Default::default()`, and every production
    path overwrote it a line later. Now `None`. All 2136 tests passed without
    modification, which is the evidence that nothing ever observed it.

  Honest about the payoff: this is an **architecture** change, not a speed one.
  A dependency chain preserves total frontend time, and the measurement agrees
  — `cargo test --no-run` is 45s against 46s before. The earlier "88% of the
  build is frontend" figure was wrong; it came from a `cargo check` run that
  had to build `.rmeta` for all 327 dependencies from scratch. Steady state,
  the lib's serial frontend is ~11s of a ~46s build.

- **CI's no-emoji guard stopped silently shrinking.** It scanned `src/**` only,
  which was correct while that held every line of Rust in the repo and quietly
  wrong the moment a module moved into a workspace crate — it would have kept
  passing over a smaller and smaller share of the code without ever saying so.
  It now walks `crates/*/src` too: 194 files instead of 135, including
  `mermaid-runtime`, which it had never covered at all.

### Fixed

- **Ollama is no longer a prerequisite for starting Mermaid.** A fresh install
  with `ANTHROPIC_API_KEY` set and no Ollama on the machine died at startup
  with "Ollama is not installed. See instructions above." plus a download
  guide — Mermaid reads as requiring Ollama even though every remote provider
  works without it. The dead end was startup model resolution: with no
  `--model`, no `last_used_model`, and no `[default_model]`, the last resort
  was Ollama's model list, and a missing Ollama was fatal there.

  Resolution now falls through to a remote provider. `[providers.<name>]
  .default_model` — a field that has been documented since it was added but
  never read by anything — becomes the startup model when its provider's key
  resolves, so `mermaid` with no arguments works on a machine that has never
  seen Ollama. Ollama still wins when it has a model: local stays the default,
  it just stopped being the requirement.

  Mermaid still never invents a vendor's model name, so the no-model-at-all
  error had to earn its keep instead: it now names the providers whose keys
  already resolve, shows the `--model` and `default_model` forms for the first
  of them, and mentions Ollama as the *local* option rather than as the thing
  you must install. `mermaid status` matches — a missing Ollama is `[INFO]`,
  not `[ERROR]`, when a remote provider is configured.

  One consequence worth naming: "which remote providers are configured" was
  computed in three places and all three missed Anthropic, Gemini, and (in
  two of them) Meta, because those have bespoke adapters and so are absent
  from the OpenAI-compatible registry that the loops walked. `doctor`,
  `status`, and startup resolution now share `providers::discovery`, which
  knows about all of them and mirrors `resolve_provider_key`'s precedence
  exactly — override env var, documented env var, Gemini's legacy
  `GEMINI_API_KEY`, then the keyring. An Anthropic-only machine used to report
  "Remote providers: none".

- **`/model` and `mermaid list` show every provider's models, not just
  Ollama's.** The same registry-only blind spot ran one level deeper: model
  *enumeration* walked the local Ollama daemon and the OpenAI-compatible
  registry, and nothing else. A user who had spent days on
  `meta/muse-spark-1.2-contributor` opened the picker and saw local models
  only — the model they were talking to was not in the list.

  `providers::discovery::provider_catalogs` now asks every configured
  provider what it serves, concurrently, speaking each one's dialect:
  `x-api-key` plus `anthropic-version` for Anthropic, `x-goog-api-key` and the
  `models[].name` shape for Gemini (filtered to `generateContent`, so its
  embedders stay out of a chat-model picker), and bearer auth over the
  OpenAI-compatible `data[].id` shape for Meta and the registry. Each request
  is best-effort with a 6s timeout, so one slow provider costs a late group
  rather than a stalled picker.

  `mermaid list` used to print provider *names* and stop; it now prints each
  provider's models under it, and says so explicitly when a catalog cannot be
  read instead of leaving a bare name that reads as "nothing here". The picker
  skips an unreadable catalog entirely — every row there is selectable, and a
  placeholder row would switch to a model id that does not exist.

- **"Which providers can this machine use" has one definition instead of
  four.** `mermaid status` printed a "Remote providers" block twice, in two
  different formats, separated by MCP servers and project instructions —
  because two of the four walks that answered the question both ran in it.
  Each walk approximated a rule that `ProviderFactory` defines, and each
  approximated it differently.

  [`ProviderFactory::resolve_provider_endpoint`] now owns that rule: it
  resolves the endpoint and credential exactly as `build_provider` would,
  without constructing an adapter or touching the network, and `build_provider`
  itself goes through it. `providers::discovery` asks it and treats `Err` as
  "not usable", so a provider is listed **iff** the factory can build one — an
  invariant a test now pins across the configs that used to split the two
  apart. `status`, `doctor`, `list`, `/model`, and startup model resolution are
  all views over that one answer.

  Two real configurations were invisible to every previous walk. A keyless
  loopback endpoint — `[providers.llamacpp] base_url = "http://127.0.0.1:8080/v1"`
  with no key, which the factory has always accepted — was dropped because no
  key resolved. A custom provider with a key but no `base_url` was reported as
  configured by one walk and not the other, when the factory rejects it
  outright. It now gets a **problem row** instead: providers you evidently
  meant to set up but that still cannot be built are listed with the factory's
  own error, on all three of `status`, `doctor`, and `list`. Cloudflare with a
  token and no `CLOUDFLARE_ACCOUNT_ID` is the canonical case.

  The status block also names each provider's endpoint, which is the one thing
  a bare provider name never told you when requests were quietly going to a
  proxy. `show_provider_status` is gone, and the Anthropic and Gemini base URLs
  and key env vars live next to their providers — one definition each, the way
  Meta's already did.

- **A dead Ollama server no longer costs ~9 seconds of silence.** With Ollama
  installed but stopped, `mermaid status` took 9.4s and `mermaid list` 9.0s
  before printing a word. The retry middleware classifies a refused connection
  as transient, so the probe ran three times with 500ms + 1000ms backoff — and
  a refused loopback connect is not free on Windows, where the SYN is
  retransmitted for ~2s before `WSAECONNREFUSED`. Three attempts plus backoff
  is the whole nine seconds.

  Retrying a refused connection could never have worked: nothing changes
  between attempts. `retry_transient_http_no_connect_retry` returns it
  immediately instead, and Ollama's `with_local_recovery` uses it for the first
  round — the round whose only useful outcome is to hand off to
  `ensure_running`. 5xx and 429 still retry under the same policy, there and in
  the post-recovery round, because those come from a server that IS running and
  may recover on its own.

  `status` is now 3.1s and `list` 2.6s, both dominated by the single connect the
  OS makes us wait for. The chat path gains the same time in a place that
  matters more: a user whose Ollama is down used to wait through the full
  backoff before the "Starting the local Ollama server…" notice appeared at
  all, and now reaches the autostart in one attempt.

## [0.20.0] - 2026-08-07

### Changed

- **Plan mode is a safety mode, not a badge on top of one.** It now sits in the
  Shift+Tab cycle as its strictest position: `plan → read_only → ask → auto →
  full_access → plan`. `Alt+P` is gone — one keystroke reaches every mode, and
  `/safety plan` works like any other level. The footer reads plain
  `safety: plan`; the old band, `plan mode on (alt+p to toggle) - restores:
  <mode>`, described the retired model where planning layered over a remembered
  mode. `PlanState.resume_safety_mode` is gone with it: a mode does not carry
  the mode before it. Leaving plan by cycling lands on the next position;
  leaving by `/plan off`, plan approval, or a handoff lands on the configured
  `[safety] mode`. Every switch routes through one `apply_safety_mode`, which
  owns the plan-file allocation on entry and the teardown on exit, so the mode
  and the read-only floor cannot disagree.

### Fixed

- **Screenshot paste works on Windows, and image paste is at parity across
  every backend.** A "copied image" is three different things depending on the
  app that copied it, and mermaid only handled one of them per platform:
  - *An encoded blob with no raster form* — GIMP, Figma, and some Electron apps
    put `PNG` on the Windows clipboard with `CF_BITMAP` absent.
    `Clipboard::ContainsImage()` answers False for exactly that payload, so
    `has_image()` said no and Ctrl+V fell through to a text read. The Windows
    read path now takes the raw `PNG` stream byte-for-byte when present (which
    also preserves alpha) and only falls back to `GetImage()` re-encoding.
  - *A file reference* — Explorer / Finder / a Linux file manager "Copy" puts
    `CF_HDROP` / `public.file-url` / `text/uri-list` on the clipboard, which no
    backend handled. All four now resolve the reference and attach the file
    (png/jpeg/gif/webp/bmp/tiff, percent-escapes decoded, capped at 32 MB so a
    stray Ctrl+C on a huge scan can't be slurped into the process).
  - *A raster handle* — unchanged, still works.

  `has_image` and `read_image_bytes` are now both defined in terms of one
  `probe_image_source`, so they cannot disagree about whether a paste is
  possible — the split that let Windows report an image it then failed to read.
  Windows also stops hard-coding `powershell.exe`: the host is resolved once,
  preferring Windows PowerShell (STA by default) and falling back to `pwsh
  -STA`, since `System.Windows.Forms.Clipboard` throws in an MTA. `System.
  Drawing` is loaded explicitly (PowerShell 7 does not auto-load it), the temp
  path is single-quote-escaped, and `tool_exists` uses `where.exe` on Windows
  rather than the POSIX-only `which`.
- **Multi-word inline code no longer renders as a row of disconnected boxes.**
  The transcript wrapper re-emitted every separator space unstyled, punching
  plain gaps through the code background — so `` `No image data found` ``
  arrived as five separate highlights, and any wrapped answer containing a code
  phrase came out visually shredded. A gap now carries the style of the span it
  came from: interior gaps of a styled run keep the run's paint, gaps between
  runs stay plain (the space in front of a link is still not underlined).
- **The question modal wraps instead of clipping.** It built its lines
  width-blind, so a long option description ran under the right border and was
  cut mid-word. Labels, descriptions, the question, notes, and footer hints now
  wrap with a hanging indent, and the height estimator measures the wrapped
  lines so the reserved zone matches what is drawn.
- **Clipboard copies toast instead of accumulating.** "Copied N chars to
  clipboard" went into the chat transcript and stayed there for the rest of the
  session. A copy is feedback on a keystroke, not conversation: it now shows
  right-aligned above the input for two seconds and expires on its own.
  Failures still go to the transcript, where they can be read after the fact.
- **Windows spawns no longer open stray console windows.** `DETACHED_PROCESS`
  gives a child *no console at all*, so the moment a console-subsystem child
  touches console I/O, Windows allocates one — and on Windows 11, where Windows
  Terminal is the default console host, that allocation opens a **visible
  window**. Redirecting stdio to null does not prevent it: the window comes from
  the console allocation, not from output. Killing the child then orphans the
  window on the desktop showing a launch error.

  Three spawn sites used it, two of them in production: ollama autostart
  (`ollama/server.rs`) and the managed-search server (`searxng/mod.rs`), plus a
  test helper that leaked one window per test-suite run. All now use
  `CREATE_NO_WINDOW`, which is what the exec tool's background launcher already
  used — same isolation (own console, exempt from Ctrl+C fan-out, survives
  mermaid exiting) with no window, and strictly more compatible, since
  PowerShell dies at startup under `DETACHED_PROCESS`. The constant is removed
  rather than left available to reach for, and `no_spawn_site_uses_detached_
  process` fails the build if it comes back. Measured: `cmd /C ping … >NUL`
  under `DETACHED_PROCESS` adds one visible top-level window, under
  `CREATE_NO_WINDOW` adds none; two full test runs after the fix leave zero.
- **Compaction works on small context windows.** Two sizing bugs made `/compact`
  impossible below roughly a 10k window, and made it *worse* as the window got
  smaller:
  - `max_input_tokens` subtracted a flat 8k summary cap from the window and then
    treated a non-positive result as "window unknown", falling back to the most
    permissive 64k budget. Measured on identical history, a 9k model got a
    2,688-char excerpt and compacted fine, while an 8k model got 16,059 chars
    and failed — shrinking the window grew the request. `summary_input_budget`
    replaces it and is monotonic in the window by construction.
  - The summarizer's own output cap was a flat 8,000 tokens regardless of the
    model. At a 4k window it asked for twice the entire window, so no budget
    could have made it fit. `summary_output_tokens` scales it to a quarter of a
    known window, with `summary_max_tokens` still the ceiling — so large and
    unknown windows behave exactly as before.

  A window genuinely too small to hold a checkpoint now skips with
  `CompactionSkip::WindowTooSmall` ("the model's context window is too small to
  hold a checkpoint") instead of building a request guaranteed to be rejected.
  The fit invariant was already tested — but only at a 32k window, comfortably
  above the cliff, which is why this went unnoticed; it is now checked at 128k,
  32k, 16k, 9k, 8k, 4k, and 2k, alongside a monotonicity test.
- **A compaction receipt and its record report the same number.** `after_tokens`
  counts a replacement whose receipt text prints `after_tokens`, and the code
  ran exactly two passes of that fixpoint, keeping the message from one
  iteration and the record from the next. The transcript receipt and
  `/context`'s "last compaction" line therefore disagreed. It now iterates to a
  fixpoint and keeps the replacement built *from* the record it reports.
- **`/clear` no longer blanks the context gauge either.** A cleared
  conversation is not one of *unknown* size — the system prompt and every
  advertised tool schema still ride the next request, routinely tens of
  thousands of tokens before you type anything. `context: n/a` hid that floor
  until the first reply came back; the gauge is now re-estimated the same way a
  rewind does. Cumulative spend still resets: those tokens were really spent,
  and that is a different number.
- **A rewind no longer blanks the context gauge.** Double-Esc to an earlier
  prompt reset `context_usage` to `None`, so a session sitting at 250k/1M
  dropped to `context: n/a` — which reads as "the meter broke" rather than
  "there is less context now", and stayed that way until the next model call.
  The fork's context is in fact the most precisely known thing about it (the
  prefix the user just picked), so it is re-estimated from the truncated
  history instead of cleared, through the same computation `/context` uses. It
  renders with the `~` estimate marker until a real call returns
  provider-counted usage. Cumulative spend still resets — those tokens were
  really spent, and that is a different number.
- **A read-only denial names the risk that tripped it.** Every refusal said
  "blocks mutations and control actions", so a `curl` was told it had mutated
  something and the model retried variations of a read. Network, process,
  external-access, and machine-scoped denials now say so.

### Added

- **`[compaction]` is now a real config section.** Nine knobs that were
  hard-coded constants — `auto_enabled`, `auto_threshold_percent`, `tail_turns`,
  `tail_token_budget`, `tool_output_max_chars`, `summary_max_tokens`,
  `summarizer_input_token_budget`, and the response-reserve bounds — are
  settable, alongside the existing `max_truncation_recoveries`. An absent
  section reproduces the constants exactly, pinned by a test.

  Values are sanitized on read rather than validated on load, so a hand-edited
  config degrades to the nearest workable number instead of refusing to start:
  the threshold clamps to `1..=100`, `tail_turns` to at least 1 (a checkpoint
  needs a live turn after it), a `0` budget falls back to its default, and
  **swapped reserve bounds are ordered rather than obeyed** — `response_reserve`
  clamps with `.max(min).min(max)`, so an inverted pair would have silently
  under-reserved on every turn.

  `CompactionRequest::manual`/`auto` now take the policy explicitly instead of
  defaulting it, so no call site can quietly ignore the user's settings — which
  is how the auto-compaction path in the effect layer had been reaching for the
  constants.
- **`/model` opens a picker instead of printing the current model.** It used to
  answer a question nobody had — the point of the command is to *change* the
  model. The pane lists everything this machine can actually reach (local Ollama
  models grouped first, then each remote provider whose key resolves), marks the
  active model with a `✔`, and narrows as you type: ↑↓ navigate, Enter switches,
  Esc cancels and leaves your draft untouched.

  The filter is the part the fixed-length pickers this is modeled on don't need
  and this one does: a provider's `/models` endpoint routinely returns 100+ ids,
  so showing a fixed handful would misrepresent what's available and showing all
  of them would be unusable. Matching is a case-insensitive subsequence over the
  id, so `oplus` finds `anthropic/claude-opus-4-5`.

  Discovery is best-effort and strictly read-only: the pane opens immediately
  and fills in, a provider that fails or times out costs its rows rather than
  the whole listing, and a stopped Ollama is **not** started — a cloud-only user
  who stopped it to free VRAM must not have it resurrected by opening a list.
  Selection routes through the same `switch_model` path as `/model <name>`, so a
  picked model still gets the vision re-probe, the persistence write, and the
  Ollama auto-pull.
- **Golden-frame tests: the whole screen, compared cell by cell**
  (`tests/pty_frame.rs`, `tests/harness/`). Three bugs shipped in one session —
  inline code rendered as disconnected boxes, a modal clipped mid-word, a status
  band advertising a removed keybinding — and every unit test passed through all
  of them, because `Line`/`Span` assertions cannot see a shredded background or
  a glyph past the border. A golden frame is the missing detector.

  Two things make it work. **Cells, not bytes:** ratatui redraws only changed
  cells and jumps the rest with cursor moves, so the byte stream is not the
  screen (`[Image #1] ` arrives split as `[Image` … `#1]`); the stream is fed to
  a real VT parser and the *grid* is captured. **Redaction, not avoidance:** a
  frame legitimately holds a version, a sandbox path, a hostname and a clock,
  each rewritten to a placeholder — including the whole cwd line, since its
  padding encodes the path's *length*, and the message timestamp, which renders
  in local time and would otherwise fail in every other timezone.

  Chat rendering is snapshot-able because the transcript is seeded on disk and
  loaded with `--resume` — no model call, fixed content. Four frames are pinned:
  startup, assistant markdown (lists, inline code, a table), the `/model` picker,
  and every safety-mode footer. `UPDATE_SNAPSHOTS=1` rewrites them; review the
  diff like any other diff.
- **`tests/pty_visual.rs` — the UI, tested through a real terminal.** Unit tests
  assert on `Line`/`Span` values, which is exactly why a wrong mode band could
  ship with every assertion passing. This spawns the actual binary on a pty
  (openpty / ConPTY) and reads the glyphs that landed on screen: Shift+Tab walks
  the whole safety cycle in the footer, and Ctrl+V against a PNG-only clipboard
  splices an `[Image #1]` token into the input.

## [0.19.1] - 2026-08-05

### Changed

- **A healthy, local web setup now starts quiet.** The startup web-capability
  line printed on every run, so the one case worth reading — traffic leaving
  the machine — looked exactly like the many that did not. The notice now
  resolves to nothing when every capability initialized AND every one of them
  terminates on this machine, so silence means "working and local". A working
  cloud backend still announces itself: viability must never buy silence for
  off-machine egress, which is the disclosure a sovereignty-focused tool most
  owes its user. Backends carry an `Egress` discriminant (`OnMachine` /
  `OffMachine`) rather than having the disclosure branch on prose, so
  rewording a trust destination can no longer mute it by accident. Anything
  that cannot be proven local discloses — an operator-configured SearXNG URL
  counts as off-machine even when it points at loopback. Remediation text for
  a failed backend moves off the headline onto its own `-` bullet, which the
  transcript renderer keeps intact (a leading indent does not survive its
  word-by-word re-wrap).

## [0.19.0] - 2026-08-04

### Changed

- **Fully-completed checklists retire at run end.** A checklist's lifetime is
  the unit of work that created it: when a run ends naturally with every
  task completed, the harness clears the store (and the tool-side broker
  mirror) instead of leaving a zombie band that re-renders on every later
  run and haunts saved sessions. The run summary line absorbs the record
  ("Worked for … · 7 tasks completed"), and lists with unfinished work still
  carry across runs. Retirement fires at natural run end and NOWHERE else —
  a cancelled or errored run keeps its list, including across `--resume`.
  Models are told finished work is retired automatically — cleanup is
  structural, not prompt discipline.

- **Plan-mode denials teach the escape hatch.** Every plan-policy denial now
  names the plan file path and the tools that can write it, instead of the
  generic "capture this change in the plan" (observed sending weaker models
  into minutes-long probe loops that never found `write_file`). The plan
  capabilities line and `PLAN_MODE_PROMPT` name `write_file`/`apply_patch`
  explicitly, the base prompt's Task Planning section and
  "do not stop at a proposal" imperative are swapped for plan-shaped stubs
  while planning (custom system prompts pass through untouched), and the
  task-checklist writers (`task_create`/`task_update`) are no longer
  advertised while a plan is being drafted — their descriptions recommended
  exactly the call the gate then hard-errors (`task_list` stays; an explicit
  `tasks = "allow"` in `[plan]` restores the writers).


- **Web egress now has one capability and policy boundary.** Native fetch is
  keyless and remains the default, but now uses a fail-closed client with no
  ambient proxies or referrers, validates the initial URL and every redirect
  against the global-unicast destination policy, rejects URL userinfo and
  HTTPS downgrades, and preserves final provenance. Web tools are omitted and
  hard-blocked by `safety.network = "deny"`; `read_only` requires one-shot web
  approval unless `allow_readonly_web` is explicitly enabled. Web actions can
  no longer be session-allowlisted, and project config can no longer select a
  web backend or destination.

- **Web results are typed and resource-bounded.** Native fetches now carry
  requested and final URL, while cloud fetches explicitly mark final
  provenance unavailable because the provider does not disclose redirects;
  all fetches carry status, MIME, charset, backend, extraction mode,
  source/extracted sizes, title, and truncation. MIME controls extraction;
  declared charsets are decoded, unsupported media and empty extraction fail
  explicitly, and the complete rendered envelope is capped at 30 KB. Runtime
  limits are eight downloads globally, two per origin, two blocking
  extractors, 16 MiB per response, and 64 MiB of decoded web data per turn,
  charged while response chunks stream rather than after a download finishes.
  Batched search runs four queries concurrently, preserves input order, and
  reports partial failures structurally.

- **Cloud web routing is explicit.** `web_search = "auto"` now means the
  managed local SearXNG backend only; the presence of `OLLAMA_API_KEY` no
  longer silently sends search queries to Ollama Cloud. Select `ollama`
  explicitly for cloud fetch/search, or configure a self-hosted SearXNG. On
  unsupported managed-search platforms, the tool is omitted with an
  actionable reason. The interactive TUI now reports the same startup-resolved
  fetch/search backend, availability, and trust destination consumed by the
  registry and subagents without re-resolving credentials.

### Added

- **Plan mode: context-delta injector.** Mode changes are now conversational
  EVENTS, not just ambient state: at every dispatch the reducer diffs the
  live plan/safety/model context against what the model was last told
  (`AdvertisedContext`, persisted with the conversation) and injects one
  hidden, persistent timeline marker per change ("Plan mode is now ON —
  author the plan at <path> using write_file or apply_patch…"). Entering
  plan mode before or during a run now reorients the model at the next
  request instead of it discovering the mode by hitting the policy gate;
  rapid on/off toggles between dispatches collapse to no marker; forked
  handoffs announce the plan's end in the NEW conversation. A compact
  plan-mode reminder also rides the history tail on every dispatch while
  planning — the position weak models reliably read — and escalates to a
  corrective after repeated plan denials with no plan write (the doom-loop
  breaker). The breaker arms and disarms on recorded FACTS, not tool names:
  only a denied MUTATION arms it (a read-only Ground phase with
  `[plan] web = deny` must not), and any call the gate approved via the
  plan-file carve-out disarms it — including the shell redirect the escalated
  corrective itself recommends.

- **Plan mode: shell spelling of plan authoring.** A command whose only
  provable effect is writing the plan file (`echo … > plan.md`,
  `printf … >> plan.md`, `cat > plan.md <<'EOF'`) is now allowed while
  planning — same exemption the `write_file`/`apply_patch` path always had.
  Worst-segment anchored and fail-closed: substitutions, `tee`/`dd`,
  variable/tilde/glob targets, chained mutations, expanding heredoc bodies
  with `$(…)`, and any command that moves the shell's cwd (`cd`/`pushd`) all
  still deny. The redirect target resolves against the directory the command
  actually runs in, so an explicit `working_dir` can no longer land a
  "plan write" on a different file.

- **Heredoc-aware shell classification.** The safety classifier no longer
  splits heredoc bodies into phantom command segments: `cat <<'EOF' …` with
  prose (or quoted shell examples) classifies by the consuming command, so
  read-only heredocs stop being denied in read-only/ask/auto modes. Bodies
  of EXPANDING heredocs (`<<EOF`, unquoted delimiter) are still scanned for
  `$(…)`/backtick substitutions — quote-blind, since heredoc bodies have no
  shell quoting context — and the raw-text destructive scan runs before any
  segmentation, unchanged. Two consequences are deliberately fail-closed
  rather than free wins: an `Allow` override anchor refuses any command
  carrying a heredoc (otherwise `allow psql` would widen to cover a whole
  `psql <<'SQL' … SQL` script body), and the hard-deny scan looks INSIDE
  heredoc and substitution bodies, so `bash <<'EOF' … nc -l … EOF` still
  blocks. A `<<` whose delimiter never terminates is not treated as a heredoc
  at all — stricter than the shell, because a misread operator that swallows
  the following lines as inert data is a read-only bypass.

- **Plan mode is a safety mode.** `SafetyMode::Plan` replaces the old
  orthogonal `Session.plan` flag; `Session.plan` now carries only the plan's
  DATA (path, saved overrides, staged resume mode). The two could previously
  disagree — Shift+Tab while planning set `full_access` live and the injector
  announced it while the plan read-only floor was still in force, which the
  model read as permission to mutate (#282). While planning, Shift+Tab and
  `/safety <mode>` STAGE the mode plan exit will restore; the status band
  shows it as `restores: <mode>`. Plan is entered with `/plan` or Alt+P only —
  it is not a position in the Shift+Tab cycle and not a valid
  `safety.mode` config value.

- **Rendering a frame no longer costs O(transcript).** Two changes, measured
  end to end on a 324 KB / 2000-message session: **1962 us -> 116 us per idle
  frame (16.9x)**, taking a 60fps idle repaint from ~11.8% of a core to
  ~0.70%. Frame time is now flat as history grows (116 us at 25 pairs, 116 us
  at 1000).
  - The frame memo already cached the assembled lines, but every frame cloned
    the whole vector (~20k `Line`s) and handed it to `Paragraph`, which drew
    40 rows and dropped the rest. Only the visible window is cloned now.
    Selection anchors are rebased onto it.
  - The memo key was a hash of every message on every frame. It is now the
    conversation's `revision` — `ConversationHistory::messages` is private and
    the accessor that hands out `&mut` bumps it, so no committed change can
    escape the key, and a stale transcript is not reachable by forgetting to
    bump. A debug-only assertion cross-checks the key against the old
    full-content hash, catching a mutation made inside the defining module
    where Rust's privacy does not reach.

  The full-frame snapshot suite is unchanged, byte for byte.

- **Zero-copy rendering survives a mode change.** The render stitch pre-pass
  runs only when a continuation actually needs merging — committed, or live
  mid-stream. Hidden-but-persistent context markers no longer force it:
  `ChatWidget` already skips those kinds, so stitching them out changes
  nothing a user can see, and a single plan toggle used to make the pre-pass
  permanent for the session. Primarily a correctness fix to a predicate that
  claimed the transform would change something when it provably would not;
  the per-frame saving is ~1% of frame time (measured), while the avoided
  full-transcript clone on every history change is the larger win.

- **Harness steering reaches every provider.** Model-directed messages
  (plan reminders, context markers, and the pre-existing auto-continue and
  stalled-turn nudges) are typed `MessageAudience::ModelDirected` and every
  adapter must deliver them. Anthropic and Gemini — whose APIs have no
  mid-conversation system role — were silently DROPPING all of them; they now
  ride a tagged `<system-reminder>` block on the adjacent user turn, keeping
  the history-tail position and leaving the cached system prefix untouched.


- **Bounded, isolated web-fetch session snapshots.** Each successful fetch returns a
  snapshot id. Follow-up calls can perform full Unicode-caseless matching or
  request stable line ranges without refetching a mutable page. Snapshot lookup
  is bound to the originating session/task, and the process-wide cache retains
  at most four entries / 32 MiB including owned metadata. The TUI and
  run-event stream now expose sanitized backend, redirect, status, MIME,
  extraction, size, match, partial-failure, and truncation provenance.

### Fixed

- **Anthropic and Gemini payloads always alternate roles.** Both APIs reject a
  history with two same-role turns in a row, and several ordinary shapes
  produced one: harness steering sitting between two user turns (a request that
  errored before any assistant turn committed, then a retype), a typed message
  arriving while tool results were pending, or two assistant turns from an
  interrupted continuation. Delivering model-directed steering on a user turn
  made the first of those reachable on every plan-mode session. Both adapters
  now coalesce same-role neighbours at the single exit point of
  `convert_messages`, so the invalid payload is unrepresentable however the
  history is shaped rather than something each match arm has to remember.
  Merged turns keep Anthropic's own block-placement rules (`tool_result` leads
  a user turn, `thinking` leads an assistant turn), so the fix cannot trade a
  role-alternation 400 for a placement one.

- **Web persistence and managed-search lifecycle hardening.** Runtime tool
  arguments and outcomes are cloned and centrally redacted before SQLite
  persistence, including signed URLs and secret-shaped fetched content.
  Managed SearXNG now retains process ownership before awaiting readiness,
  health-checks cached children through a bounded challenge-response JSON
  probe, reaps failed/cancelled starts, bounds bundle downloads and decoded
  archive expansion, rejects unsafe archive entries, serializes multi-process
  provisioning through a crash-released advisory transition lock with
  nonce-scoped ownership and exact stale-snapshot revalidation, fails closed
  without secure entropy, and retains the transition claim through atomic
  immutable-generation publication. Heartbeats atomically replace complete
  owner records, lock quarantine is durably published before claim release,
  and extracted trees are recursively synced before publication. Retained
  generations and hard-crashed download staging are capped without deleting
  trees that another process may still use. CI now exercises the real managed
  bundle end to end on every supported Linux/macOS OS family.

- **Dead code and duplication swept out of the tree.** Removed unreferenced
  items (`path_stem`, `snapshot_field_from_daemon`, `arc_sink`,
  `clear_selection`, `is_manually_scrolling`, dead `type_` wire fields) and
  the stale `#[allow(dead_code)]` markers on fields that are in fact read.
  The four provider wrappers no longer carry byte-identical copies of the
  adapter stream callback — it lives once in `stream_bridge::forward_callback`
  — and the duplicated SearXNG interpreter-path helper, the duplicated Ollama
  Cloud backend construction, and the duplicated daemon request/response
  exchange each collapse to a single definition. `parse_fetch_args` now
  returns a `FetchTarget` enum, so "exactly one of url/snapshot_id" is
  enforced by the type rather than by a downstream `expect`. The crate-wide
  `clippy::collapsible_match` suppression is gone, its six sites fixed, and
  `test_exec_context_with_config` now honors `config.safety.mode` instead of
  silently forcing `FullAccess`.


## [0.18.0] - 2026-07-21

### Changed

- **BREAKING: Windows commands now run under PowerShell instead of `cmd`.**
  Foreground and background `execute_command` on Windows spawn PowerShell
  (`pwsh`, falling back to Windows PowerShell 5.1) with `-NoProfile
  -NonInteractive` rather than `cmd /C`. Write commands in PowerShell syntax
  (`$env:VAR`, `Get-ChildItem`, `Select-String`); `cmd`-only syntax
  (`%VAR%`, `if defined`, `&` chaining) no longer works. Native exit codes
  now propagate — a failing `cargo build` surfaces its real code instead of
  collapsing to 0/1 — and background processes get a hidden console so they
  survive detached. The read-only-command allowlist and the
  destructive-command hard-deny both learned the PowerShell spellings
  (read-only cmdlets like `Get-Content`/`Test-Path`, and blocked shapes like
  `Remove-Item -Recurse` on dangerous roots).

- **BREAKING: write-capable MCP tools are vetted even in `full_access`.** A
  new `[safety] external_writes` floor (`allow` | `auto` | `ask` | `deny`,
  default `auto`) means safety mode alone no longer authorizes an external
  side effect: an MCP tool without a server-advertised `readOnlyHint` is
  vetted against your request by the intent classifier — aligned calls run
  silently, off-task calls escalate to approval — in every mode,
  `full_access` included. Read-annotated tools are unaffected, and the hint
  is treated as untrusted (it never grants more than the mode already
  allows). Set `external_writes = "allow"` to restore the previous
  unconditional behavior.

- **BREAKING: machine-scoped package installs are vetted even in
  `full_access`.** A new `[safety] system_installs` floor (same levels and
  `auto` default) gates system-scoped package operations — `npm -g`, `cargo
  install`, `pip install`, `pipx`, `gem install`, `go install`, `dotnet tool
  install`, and `brew`/`apt`/`dnf`/`pacman`/`winget`/`scoop`/`choco`/…
  installs — because they mutate the machine, not the project, and sit
  outside checkpoint reach. Project-local installs (`npm install`, `cargo
  add`, `yarn add`) are untouched. Set `system_installs = "allow"` to restore
  the previous behavior.

### Added

- **`blocked` task-checklist status.** A task stalled on an external
  dependency can be marked `blocked` — rendered with its own glyph and
  overflow-footer count — instead of masquerading as pending. Unblocking a
  task preserves its original start time and token accounting. The blocker
  workflow the agent follows: mark the stuck task blocked, add a task for the
  blocker, and make that one in-progress.

### Fixed

- **System prompt corrected against runtime behavior.** The agent's system
  prompt is now truthful about what the runtime actually does: the real
  per-platform shells, the exact tool inventory (memory verbs,
  `ask_user_question`, `tool_search`-deferred MCP tools), the read-only
  mode's true allow/deny surface, scratchpad sweep timing, plan-mode gating,
  and the reduced subagent toolset. Memory facts are now readable via
  `read_file` in every scope, the `write_file` schema no longer points at a
  removed tool, and the `memory` tool reports its `search` action. New
  regression guards hold the prompt, the README's slash commands, and the
  sample config to the runtime so they cannot silently drift again.

## [0.17.0] - 2026-07-13

### Added

- **Windows: PTY-fidelity foreground exec via ConPTY.** Foreground
  `execute_command` on Windows now runs on a real pseudoconsole (the same
  portable-pty path Unix uses) instead of always degrading to pipes: the
  child sees a console (`isatty`-gated color and progress tools behave),
  and the capture reaches the model ANSI-stripped and CRLF-normalized.
  `[exec] pty = false` still forces pipes, and any pre-spawn PTY failure
  falls back to the pipe path. `strip_ansi` now also consumes
  DCS/SOS/PM/APC string payloads, drops bare BEL, and applies backspace
  erasures — artifacts ConPTY repaints emit.

- **macOS exec sandbox (Seatbelt).** `--no-network` and `--confine-fs` /
  `--sandbox` now confine model-run shell commands on macOS via
  `/usr/bin/sandbox-exec`, behind the same launcher that applies
  seccomp/Landlock on Linux. The generated allow-default profile denies
  network (sparing `AF_UNIX` sockets, matching Linux) and denies writes
  outside the allowed roots, matching both the literal and canonicalized
  path so macOS `TMPDIR` firmlinks work. Fail-closed: if the sandbox cannot
  be applied the command exits 126 instead of running unconfined.
  `mermaid self-test` now reports real per-platform sandbox availability
  instead of a hardcoded "yes" off-Linux, and CI gains a macOS
  sandbox-integration job with a Seatbelt profile-compile canary.

- **Plan mode: headless runs + SDK visibility.** `mermaid run --plan` enters
  plan mode before the prompt seeds: the run explores read-only and
  delivers a plan file (`.mermaid/plans/`) as its result — with no approval
  UI the plan is accepted but implementation does not start.
  `--plan-autoaccept` flows the same run straight from the approved plan
  into implementation. `RunEvent::ToolFinished` gains an additive `plan`
  payload (path + start/fresh/fork disposition) when the finishing call is
  `exit_plan_mode`, so NDJSON streams and daemon `subscribe_task`
  subscribers get first-class plan visibility without any change to
  existing event shapes.

- **Plan mode: clear-context execute + handoff.** The approval dialog gains
  `Approve, clear context and start` — execution continues in a fresh
  conversation seeded with a handoff preamble plus the approved plan (the
  option shows how full the context is, so the tradeoff is legible), with
  the checklist re-seeded and the exploration transcript saved to disk —
  and `Hand off...`, a two-step sub-dialog: fork of this session (carries
  the transcript) or fresh session, then the execution model (same model,
  a recently-used or locally-pulled model from `ollama list`, or any
  free-typed `provider/model`). Handoffs mint a new conversation id (the
  rewind-fork machinery), switch models per-request, and auto-start
  implementation.

- **Remote MCP servers over Streamable HTTP.** `[mcp_servers.<name>]` now
  accepts `url = "https://…"` instead of `command`, connecting to hosted MCP
  servers (GitHub/Sentry/Linear-class) everywhere MCP works today — TUI,
  headless, and daemon. Auth rides as headers: literal `headers` in config,
  or `env_headers` mapping a header to an environment variable resolved at
  request time so the secret never lands in config.toml. Register from the
  CLI with `mermaid add <name> --url https://… --header 'Name: Value'
  --env-header 'Authorization=TOKEN_VAR'`. The transport tracks the server's
  `MCP-Session-Id`, sends the negotiated `MCP-Protocol-Version` on every
  request after initialize, drains SSE-streamed responses (including
  reconnect-and-resume when the server closes a stream mid-request), and
  DELETEs the session on shutdown. URLs must be `https`, or `http` to
  loopback only; connections to private/link-local addresses are refused
  unless the server entry opts in with `allow_private_network = true`.

- **Run summary shows total line changes.** The end-of-run
  `Worked for … · used … tokens` line now appends ` · +N/-M` — the exact
  lines added and removed across every file mutation in the run
  (`write_file`, `apply_patch`), so you don't have to sum each tool call's
  diff by hand. Counts come from the tools' exact diff metadata (not the
  display diff, which is capped at 220 lines), and the segment is omitted
  entirely on runs that changed nothing.

- **Plan mode: permission profile picker + plan/execute model split.**
  `/plan config` (or `/config`) opens a settings picker: a top-level
  permissions preset (`default` / `strict` / `open`, showing `custom` when
  granular values diverge) over per-category levels — builds/tests, web,
  memory writes, task tools — each cyclable through allow / auto (classifier
  -vetted) / ask (approval modal) / deny, applied live at the policy gate on
  the very next tool call and persisted to the `[plan]` config table. The
  plan-mode system prompt now derives its "what runs while planning" line
  from the live profile so it never promises what the gate will deny. New
  `[plan] model` and `[plan] reasoning` overrides: entering plan mode swaps
  the session onto the plan-phase model/effort and leaving (approve or
  cancel) restores the previous one — plan on a frontier model, execute
  locally, or invert for privacy.

- **Plan mode: approval flow + task seeding.** The model now finishes a plan
  by calling `exit_plan_mode`, which re-reads the plan file from disk (your
  external edits win) and raises an approval dialog: `Approve and start` /
  `Approve and wait` / `Request changes` (with a typed note that goes back
  to the model verbatim). Approval leaves plan mode, restores your previous
  safety mode, **seeds the live task checklist from the plan's Tasks
  section** — re-approving a revised plan reconciles instead of resetting
  (completed items survive) — renders the approved plan into the transcript
  as a markdown block, and (if you chose start) auto-submits
  `Implement the plan.` so approval flows straight into implementation.
  A new `enter_plan_mode` tool lets the model propose planning first; each
  plan tool is advertised only in the mode where it applies. New `[plan]`
  config: `auto_approve` (skip the dialog entirely, default false) and
  `post_approve = "start"|"wait"` (pin what approval does; unset keeps
  both options in the dialog).

- **Plan mode (core).** Alt+P or `/plan` puts the session in a read-only
  collaboration state for designing work before doing it: tool dispatch
  floors the effective safety mode to read-only at the policy gate (hard
  enforcement, not prompt-only), with exactly three carve-outs — the plan
  file itself (`.mermaid/plans/<slug>.md`, project-local), memory writes,
  and known-safe build/test commands (`cargo check/build/test/clippy`,
  `go build/test/vet`, `npm test`, `make test`, …; anchored like Allow
  overrides, so substitutions, wrappers, redirects, and mutating tail
  segments refuse). The model gets a short plan-mode charter (three phases,
  decision-complete quality bar, five-section plan format whose Tasks list
  will seed the live checklist on approval) and plan-flavored denials that
  teach instead of frustrate; leaving plan mode neutralizes stale denials
  the way safety-mode loosening already does. The task checklist writers
  are firewalled during planning. Status bar shows
  `plan mode on (alt+p to toggle) - restores: <mode>` — the previous
  safety mode is untouched and resumes on exit. Planning-in-progress
  survives `--resume`. Approval flow, task seeding, per-category plan
  permissions, model split, and handoff land in follow-up PRs.

- **Per-session scratchpad.** Every session now gets a private 0700 scratch
  directory under the system temp dir
  (`<temp>/mermaid-<uid>/<project-slug>/<session-id>/scratchpad`), recreated
  on `/clear`, `/load`, and rewind forks.
  Shell commands receive its absolute path as `MERMAID_SCRATCHPAD`, the
  file tools accept absolute paths inside it, and subagents share the
  parent session's directory. Writes there are never checkpointed and skip
  approval gating (read-only mode still blocks them), making it the cheap
  place for intermediate artifacts. New `/scratchpad` command prints a
  bounded listing, `/doctor` reports the path, deleting a saved
  conversation removes its scratchpad, and stale unlocked directories are
  swept on session startup (mermaidd tunes the window via
  `daemon.scratchpad_retention_days`).

- **Native structured output on Anthropic.** `mermaid run --output-schema`
  now sends the schema as `output_config.format` (JSON Schema) on the
  Anthropic formatting turn, matching the native enforcement already used
  for OpenAI-compatible providers, Gemini, and Ollama. Anthropic accepts a
  JSON-Schema subset and older models reject `format` entirely; in either
  case the run keeps the agent's original answer, records an
  `output_schema:` error, and the client-side validator remains the final
  gate.


- **Ctrl+J inserts a newline in the input box.** Multi-line prompts can now
  be composed directly: Ctrl+J breaks the line at the cursor (matching
  Claude Code and Codex), Enter still submits. Works on legacy terminals
  too — the chord needs no kitty keyboard enhancement. Listed under
  `/help` keybindings.

- **Plugin bundles: MCP servers, prompt commands, and agent types.** An
  enabled plugin's `plugin.toml` can now declare `mcp` (TOML files of
  `[servers.<name>]` configs, started with your own servers and flowing
  through tool deferral), `prompts` (markdown slash commands with
  `$ARGUMENTS` substitution, shown in the palette as `(plugin:<name>)` and
  in `/help`), and `agents` (TOML files of `[types.<name>]` agent types).
  Config-defined entries always shadow plugin ones with a warning;
  built-in slash commands can never be shadowed; `./`-relative MCP
  commands resolve inside the plugin directory with containment. Same
  restart-to-refresh policy as skills.

- **Live task streaming: `mermaid task <id> --follow`.** Attach to a daemon
  task's `RunEvent` stream — an ack line, then NDJSON events until the
  terminal `result`. Subscribing to a still-queued task works (the
  subscriber holds a receiver on the same channel the executor later uses);
  an already-finished task gets one synthesized result from the persisted
  record; slow clients are dropped by a per-write timeout so they can't
  block the daemon; lagged subscribers get an in-band error event.


- **Auto-continued replies are now seamless.** A reply that crosses the
  model's per-response output cap reads as ONE uninterrupted assistant
  bubble: the transcript stitches the continuation into its bubble (a code
  fence cut open at the seam renders as a single intact block), a
  conservative overlap trim drops the resume-echo some models emit, and the
  in-flight continuation streams into the same bubble live. The "continuing"
  system note is gone from view and — once its one request has gone out —
  retired from history entirely, so it can never steer a later, unrelated
  turn (the stalled-turn retry nudge gets the same treatment). Canonical
  history still stores the true wire-level messages (provider-correct,
  thinking-signature-safe); the stitch is display-only.
- **Data-driven model-capability catalog.** Thinking wire shape (anthropic
  adaptive-vs-budget, gemini level-vs-budget floors, gpt-oss think-string),
  temperature support, effort-tier ceiling, vision markers, static context
  windows, and documented output ceilings now live in one first-match-wins
  const table (`src/models/catalog.rs`) instead of model-name string gates
  scattered across four adapters and the domain. Every known model's behavior
  is pinned unchanged by table-driven tests; Ollama's `/api/show` probe stays
  authoritative for local models. Two deliberate accuracy fixes: matching is
  uniformly case-insensitive, and gpt-5/gpt-4.1's 400k static window now
  applies through any provider (previously `openai/` only).
- **Cloudflare Workers AI is now a built-in provider.** Reach Cloudflare-hosted
  models through their OpenAI-compatible endpoint with
  `mermaid --model cloudflare/@cf/<vendor>/<model>` — for example
  `cloudflare/@cf/zai-org/glm-5.2` (GLM-5.2). Set `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID` (the account id is spliced into the endpoint URL), or
  point `[providers.cloudflare].base_url` at a full account-scoped URL / AI
  Gateway endpoint. Exposes the reasoning-level selector and streams GLM-5.2's
  thinking trace. Discovery surfaces (`doctor`, the best-effort `/models` probe)
  use the same account-scoped URL and report a missing account id instead of
  probing a placeholder; a setup missing both env vars gets one error naming
  both.
- **Project-local config.** A repo can now commit `.mermaid/config.toml` at its
  git root; it layers between your user config and session flags. Safe by
  construction, with no trust prompt: security-sensitive keys (`mcp_servers`,
  `providers`, `agents`, `daemon`, `last_used_model`, `web.searxng_url`,
  `ollama.host`/`port`, and most of `safety`) are stripped with a warning, and
  the allowed `safety.mode`/`network`/`filesystem` can only TIGHTEN your user
  settings — a cloned repo can pick models and UX defaults but can never spawn
  commands, redirect prompt traffic, or relax approvals. Startup prints a
  one-line notice whenever a project config contributes keys, and runtime
  memory-setting re-reads honor the project layer too.

- **Layered config engine.** Configuration now merges as ordered layers —
  built-in defaults < `~/.config/mermaid/config.toml` < session flags (`-c`
  plus `--no-network`/`--confine-fs`/`--sandbox`, `run --max-tokens`,
  `run --allow-untrusted-tools`) — through one recursive TOML deep-merge and a
  single typed deserialize. Unknown-key warnings name the layer that contains
  the typo, and in-app settings changes now rewrite only their own keys in the
  user file: unrecognized keys survive persists, defaults are no longer frozen
  into the file, and per-model entries whose ids contain dots
  (`gemini/gemini-2.5-pro`) persist correctly. A corrupt config file degrades
  to defaults while the session flags still apply.
- **Long responses auto-continue across the model's per-response output cap.**
  When a reply is cut by the provider's per-response output ceiling with
  context-window room to spare, mermaid now continues it in a fresh turn (the
  committed partial rides in history and a note nudges the model to resume, not
  restart), bounded per run so a re-truncating model can't loop. A mid-reasoning
  cutoff or a capped run still stops with the accurate output-limit message. So
  an answer that "wants 40000 tokens" completes even on providers that cap
  single responses lower.
- **Live model-limit discovery — `Context: unknown` gets real numbers.** Most
  OpenAI-compatible providers attach the model's context window and output
  ceiling to their `/models` metadata (OpenRouter, Cloudflare, …); mermaid
  previously threw that data away. It's now parsed, cached across sessions in
  `provider_probes`, and refreshed into the live capability snapshot — so the
  status bar shows a real window for remote models, proactive auto-compaction
  works for them, the truncation classifier gets real windows, and
  `mermaid model-info` gains an `Output limit:` line. Anthropic models report
  their documented window/ceiling statically.
- **Truncation is now diagnosed correctly — no more false "Context window
  full".** A `length` stop is classified from the response usage: hitting the
  per-response output cap (window still has room — the common case on remote
  providers) now stops with an accurate message naming the real limit, instead
  of misreporting a full window and looping through futile conversation
  compactions that couldn't help. A genuinely full window still compacts and
  continues exactly as before.
- **Model-scaled output budget — the hardcoded 4096-token cap is gone.**
  `default_model.max_tokens` now defaults to `0` = **auto**: OpenAI-compatible
  providers and Gemini get no cap at all (the provider applies the model's own
  per-response maximum), Ollama's `num_predict` gets the full room the context
  window leaves after the prompt, and Anthropic (which requires `max_tokens`)
  gets the model's documented output ceiling clamped to the window. Reasoning
  models like GLM-5.2 can finally emit tens of thousands of thinking tokens
  without tripping a stale 2024-era limit. A positive `max_tokens` remains an
  explicit hard cap (cost control); existing config files carrying the frozen
  legacy `4096` are migrated to auto on load. Compaction's response reserve is
  now reasoning-aware instead of mirroring the send-cap.
- **Optional filesystem write-confinement for shell commands (Linux).**
  `mermaid --confine-fs …` (or `safety.filesystem = "project"`) confines
  model-run shell commands with a Landlock ruleset: writes are allowed only
  beneath the project directory, the system temp directory, and `/dev`; reads
  and execution stay unrestricted. A failure matching the denial signature
  while confinement is active is reported with a clear "filesystem sandbox"
  explanation and the `denied_by_sandbox` marker. `mermaid --sandbox …` is
  shorthand for `--no-network --confine-fs`. Best-effort by design: kernels
  without Landlock (pre-5.13) and other platforms degrade to a warned no-op.
  Off by default.
- **Optional network kill-switch for shell commands (Linux).** `mermaid
  --no-network …` (or `safety.network = "deny"`) confines model-run shell
  commands with a seccomp-BPF filter that denies internet sockets
  (`AF_INET`/`AF_INET6`) while leaving `AF_UNIX` and local IPC working, so a
  sandboxed command can't reach the network but ordinary local work still runs.
  A blocked attempt is reported with a clear "blocked by the network sandbox"
  message and a `denied_by_sandbox` marker instead of a confusing crash. Applied
  via a hidden `__sandbox-exec` re-exec launcher; a no-op on macOS/Windows
  (Seatbelt/AppContainer are follow-ups). Off by default.
- **Typed NDJSON event stream for `mermaid run`.** `mermaid run --format ndjson`
  streams the run lifecycle as one JSON object per line — `session_started`,
  `text`/`reasoning` deltas, `tool_started`/`tool_finished`, `approval_required`,
  `turn_done`, and a terminal `result` — so `mermaid run` can be driven as an
  SDK/subprocess. The event shape is a stable, versioned contract
  (`protocol_version`) pinned by a golden serialization test; `--format json`
  now emits that same typed `result` object.
- **Startup process hardening + per-turn timing.** On Linux, Mermaid now disables
  core dumps (`RLIMIT_CORE=0`) and ptrace attachment (`PR_SET_DUMPABLE=0`) at
  startup, so a crash can't leave a core file carrying secrets and the process
  isn't trivially attachable. Each model turn also emits a structured timing
  event (turn id, model, elapsed) to the log / diagnostic bundle.
- **Faster session listing + session provenance.** Each saved session now writes
  a tiny `<id>.meta` sidecar, so listing sessions (`/list`) reads metadata
  instead of parsing every transcript (older sessions fall back to a full read).
  Sessions also record their creation environment — git branch, git SHA, and CLI
  version — plus `forked_from` / `parent_session` lineage fields (ready for a
  future fork/rewind).
- **TUI: attention bell, keyboard scrolling, and a shortcut list.** The terminal
  title now reflects run state (`mermaid · working`, else the conversation
  title), and Mermaid rings the bell when a run finishes or an approval is
  waiting while the terminal is unfocused. PageUp/PageDown scroll the transcript
  (Shift+Up/Down by a line, End jumps back to the newest message), and `/help`
  now lists the keyboard shortcuts.
- **Providers & MCP: keyless local servers, env-sourced headers, per-server tool
  filters, raw command servers.** Local OpenAI-compatible endpoints (llama.cpp,
  vLLM, LM Studio) now work with no API key when the base URL is loopback/LAN;
  a missing key for a public provider gives an actionable "get a key at …" hint.
  Providers can source secret headers from env vars (`[providers.x] env_headers`)
  instead of writing them into config. Each MCP server accepts `enabled_tools` /
  `disabled_tools` to scope which of its tools reach the model. And `mermaid add
  --command <cmd> --arg … --env K=V` registers an arbitrary command MCP server
  without going through the package registry. (Also fixes a latent bug where a
  built-in provider's static analytics headers were dropped.)
- **Config: typo warnings, `-c` overrides, and stdin prompts.** Unknown keys in
  `config.toml` now warn with their dotted path instead of being silently
  ignored. A repeatable `-c key.path=value` flag overrides any config value for
  one invocation (`mermaid -c default_model.max_tokens=8192 run "…"`; the value
  is parsed as TOML). And `mermaid run` reads its prompt from stdin when given
  `-` or no prompt (`echo "explain this" | mermaid run -`); piped stdin
  alongside an explicit prompt is appended as a fenced block.
- **Sharper prompt + searchable memory.** The system prompt gained a `## Web`
  section (browse for time-sensitive or externally-verifiable facts, prefer
  primary sources, cite inline) and a memory "signal gate" (save a fact only if
  a future session would act better for it). The `memory` tool gained a `search`
  action that substring-matches across fact names, descriptions, and bodies — a
  targeted read that stays available in every safety mode.
- **`apply_patch` — robust, multi-file editing.** A new patch-based editor
  (`*** Begin Patch … *** End Patch` with Add / Update / Delete / Move hunks and
  optional `@@` context anchors) backed by a graduated fuzzy matcher that
  tolerates whitespace and curly-quote drift, so an edit no longer fails on a
  stray trailing space. It edits several files in one call under a single
  checkpoint, and warns when a hunk matched fuzzily. Replaces `edit_file` (the
  old single exact-match replacement is removed).
- **The daemon now schedules its tasks instead of stampeding.** `run` requests
  enqueue; a scheduler executes queued tasks bounded by
  `[daemon] max_concurrent_tasks` (default 1 — one agent run at a time, honest
  for a single local GPU), ordered by priority (`run` accepts
  `"priority": "low"|"normal"|"high"`) then FIFO. The queue is durable: tasks
  submitted while the daemon is down or busy survive restarts and drain
  automatically.
- **Daemon tasks can be cancelled.** New `cancel_task` daemon command and
  `mermaid cancel <task-id>` CLI: a running task gets the same graceful
  teardown as pressing Esc in the TUI (current tool's process tree killed,
  turn unwound, status `cancelled` persisted), with a hard stop if it doesn't
  unwind within a grace window; a queued task is cancelled before it starts.
- **Per-task wall-clock budgets.** `[daemon] task_timeout_minutes` bounds each
  daemon task's runtime (unset keeps the existing 20-minute headless default),
  so an unattended run's worst case is bounded.
- **The agent can ask you structured multiple-choice questions.** A new
  `ask_user_question` tool lets the model pause mid-run and pose 1–4 questions in
  an interactive terminal modal — single-select, multi-select, rank, or typed
  inputs (text/number/date/path) — each with an "Other" free-text escape,
  optional side-by-side previews (including diffs), and a "remember this answer"
  toggle that persists settled preferences across sessions. Answers flow back as
  the tool result so the run continues with your decision; headless runs with no
  TTY proceed with best judgment instead of blocking.

- **NVIDIA NIM is now a built-in provider.** Reach NVIDIA-hosted models through
  their OpenAI-compatible endpoint with `mermaid --model nvidia/<model>` and an
  `NVIDIA_API_KEY` — for example `nvidia/z-ai/glm-5.2` (GLM-5.2). Reasoning-model
  traces (streamed as `delta.reasoning_content`) are surfaced, and tool calling
  works as with any built-in provider.

- **Mermaid warns when you attach an image to a model that can't see it.** Some
  Ollama models have no vision capability, so a pasted image is silently ignored
  — which looks exactly like a bug. Mermaid now probes the model's advertised
  `vision` capability (Ollama `/api/show`) the moment you paste an image (and on
  `/model` switch), and posts a one-line notice *before* you send if the model
  can't see it, so you can switch to a vision-capable model instead of wasting a
  turn. The notice is shown once per model per session; a per-turn re-check backs
  it up for a fast paste-then-send. This also makes `/doctor` report Ollama
  vision support accurately instead of always claiming "no".

- **Pasted images are now inline `[Image #N]` tokens in the prompt.** Instead of
  a separate `[Image #1] (PNG, 1KB)  (↑ to select)` bar floating above the input
  box, Ctrl+V splices an inline `[Image #N]` pill into the message text at the
  cursor — you can type around it and it deletes as a unit (Backspace on the pill
  removes both the token and the image). `N` is a **stable, conversation-global**
  number (it keeps climbing across messages and survives `--resume`/`--continue`),
  so "in image #16 you can see…" is unambiguous for you and the model; the
  submitted text carries the tokens so the model can correlate each image with
  its reference, and the transcript shows the same number. This also retires the
  attachment-focus bar entirely, so the up-arrow always steps through prompt
  history with no contention.

- **`read_only` safety mode now permits `web_search` and `web_fetch`.**
  Searching and fetching the public web are reads — reading is what
  read-only mode is for — so they no longer die with "blocks mutations and
  control actions". The SSRF guard (refusing internal / loopback / metadata
  hosts) lives in the web tools and applies in every mode, and an operator
  `Deny` override on the `web` category still outranks the carve-out.
  Anything that *acts* on the network keeps the `network` category and stays
  blocked.

- **`web_search`'s managed backend is now sovereign — no Docker or Podman.**
  The default `auto` backend (when `OLLAMA_API_KEY` is unset) no longer runs a
  SearXNG container. Instead the first search downloads a self-contained,
  sha256-verified bundle — a portable CPython plus the Granian server and
  SearXNG — from [mermaid-searxng](https://github.com/noahsabaj/mermaid-searxng),
  unpacks it under the data dir, and runs Granian bound to loopback, reaped on
  mermaid exit. No container runtime, no VM, nothing to install; the bundle is
  fetched once and cached. Forcing your own instance (`search_backend =
  "searxng"` / `searxng_url`) or Ollama Cloud (`OLLAMA_API_KEY`) is unchanged.

- **The Ollama auto-start is no longer silent.** At the moment mermaid
  commits to spawning `ollama serve`, one line — "Starting the local Ollama
  server (it stays running after mermaid exits)…" — now reaches the user:
  as a system line in the TUI transcript (via a new out-of-band
  `StreamEvent::Status` → `Msg::TransientStatus` path, recorded/replayed
  like any other Msg), on stderr for headless `mermaid run` (stdout stays
  clean for the response payload), and on stderr for the startup model
  check. The line fires only when a spawn actually happens — never when the
  server was already up, never on remote URLs, never from the read-only
  verbs — closing the latency-feedback, discoverability, and consent gaps
  of a revival that can otherwise hide up to ~15s behind a generic spinner
  and leave behind a detached server with no breadcrumb.

### Changed

- **Quiet system notices.** Toggling plan mode no longer writes transcript
  rows ("Planning: … — plan mode on", "Plan mode off — safety mode: …") —
  the status band under the prompt is the single source of truth for the
  live mode, so a toggle just updates that line. Every remaining system
  notice (VRAM/vision warnings, MCP errors, background-agent completions,
  command replies) drops the colored role bullet and right-aligned
  timestamp and renders as indented muted-gray meta text, so transcript
  furniture never competes with the conversation.

- **Typed daemon control protocol.** Every `mermaidd` socket command now
  parses into one exhaustive `DaemonRequest` enum (wire shape unchanged —
  this is a contract made exhaustive, not a compat shim): a malformed
  request answers with a serde error naming the field, the auth matrix is
  compiler-checked per variant, and the CLI client constructs requests from
  the same enum so a misspelled command is a compile error.


- **Kill background agents: `/agents` command + `agent` tool kill action.**
  `/agents` lists every detached (Ctrl+B backgrounded) subagent with its id,
  description, live activity, elapsed time, and token count; `/agents kill
  <id>` cancels one and `/agents kill all` cancels every one. The model can
  manage its own children too: the `agent` tool now takes `action: "kill"`
  plus an `agent_id` (killing an already-finished child evicts it from the
  continuation cache instead). A killed child unwinds orderly, posts a
  "cancelled" note with its billed token spend folded into the session
  totals, and — unlike a normally-finished background agent — does not queue
  its partial report for the model. Closes the follow-up from the agent
  backgrounding work below.


- **Kill background agents: `/agents` command + `agent` tool kill action.**
  `/agents` lists every detached (Ctrl+B backgrounded) subagent with its id,
  description, live activity, elapsed time, and token count; `/agents kill
  <id>` cancels one and `/agents kill all` cancels every one. The model can
  manage its own children too: the `agent` tool now takes `action: "kill"`
  plus an `agent_id` (killing an already-finished child evicts it from the
  continuation cache instead). A killed child unwinds orderly, posts a
  "cancelled" note with its billed token spend folded into the session
  totals, and — unlike a normally-finished background agent — does not queue
  its partial report for the model. Closes the follow-up from the agent
  backgrounding work below.

- **OS-keyring API keys + `mermaid login`.** `mermaid login <provider>`
  stores a provider API key in the OS keyring (macOS Keychain, Windows
  Credential Manager, Linux Secret Service); `mermaid login` lists every
  provider's key status; `mermaid logout <provider>` removes a stored key.
  Environment variables keep absolute precedence, and a per-provider
  `api_key_env` override remains authoritative (no keyring fallback).
  `doctor` and `mermaid feedback` now report each key's source (`env`,
  `keyring`, `none`). `MERMAID_NO_KEYRING=1` disables keyring lookups.

- **Foreground commands run on a PTY (Unix).** `execute_command` children
  now see a real terminal: `isatty` is true, spinner-heavy tools emit sane
  progress instead of dumping escape garbage, and `/dev/tty` resolves to the
  CAPTURED pty — a `sudo`-style prompt lands in the tool output instead of
  painting over the TUI. ANSI sequences are stripped from what the model
  sees and PTY `\r\n` line endings are normalized; the tee log keeps raw
  bytes so backgrounded (`Ctrl+B`) tails render colors. The same sandbox
  launcher, secret-env scrubbing, timeout, cancel, and detach semantics
  apply on both paths. Opt out with `[exec] pty = false`; any pre-spawn PTY
  failure falls back to pipes automatically. One semantic change: a child
  that reads stdin now hangs until its timeout instead of hitting instant
  EOF (mitigated by `GIT_TERMINAL_PROMPT=0` and the command timeout).

- **Live agent panel + agent backgrounding.** Running `agent` tools now get
  one stable panel row each under the status spinner — description, the
  child's current tool or phase, elapsed time, and a live token count — so
  parallel agents are visible from the moment they start instead of only
  when they finish. Ctrl+B now genuinely works for agents: it detaches every
  running subagent from the turn (the model gets an immediate "moved to
  background" outcome), the children keep running with their rows marked
  `bg`, and each report is delivered to the conversation through the
  queued-message path when it finishes, with its token spend still folded
  into the session totals. The `ctrl+b to background` hint only renders when
  something running can actually background (shell commands or agents).

- **Named config profiles.** Define `[profiles.<name>]` overlays in your
  user config and select one per invocation with the global
  `--profile <name>` flag. Profile values beat the user file but lose to a
  repo's project config (its tighten-only safety clamp still wins) and to
  `-c` overrides. Unknown profile names error listing what is defined;
  `doctor` shows the active profile; persists never touch `[profiles.*]`.

- **`mermaid run --output-schema <FILE>`.** Structured output for headless
  runs: the agentic loop runs completely normally, then one extra formatting
  turn (no tools) reshapes the final answer to the given JSON Schema —
  natively enforced on OpenAI-compatible providers (`response_format`),
  Gemini (`responseJsonSchema`), and Ollama (`format`); prompt-driven on
  Anthropic. The response is validated client-side either way: valid output
  lands in the result's `structured_output` field, and any failure keeps the
  text answer and records an `output_schema:` error instead of returning
  nothing.

- **Task checklist (todo tracking).** New `task_create` / `task_update` /
  `task_list` tools let the model plan multi-step work as an id-addressed
  checklist: batch creation and differential batch updates in single calls,
  an optional `explanation` surfaced to the user on scope pivots, and
  soft-corrective notes (never rejections) when discipline slips (two tasks
  in_progress, pending-to-completed jumps). The checklist renders live under
  the status line — the spinner headline becomes the active task's present
  tense form, completed rows collapse to strikethrough with a per-task cost
  suffix (elapsed time + tokens), long lists window with an overflow footer —
  and Ctrl+T toggles a one-line `Next:` collapsed view. The list persists
  with the session (resume restores it), clears on rewind/fork and `/clear`,
  and subagents get isolated checklists for free. Beyond the tools: a
  bounded evidence trail records what actually ran while each task was in
  progress (visible in `task_list` and `/todos`); a staleness nudge tells
  the model when an in_progress task has gone untouched for five model
  calls; the gated `task_completed` plugin hook can veto a completion
  (flipping the task back to in_progress with the reason); `/todos`
  (`add`/`rm`/`done`/`clear`) lets the user edit the checklist directly,
  with the model notified on its next request; and headless runs emit an
  additive `tasks_updated` NDJSON event carrying the full snapshot with
  cost attribution.

- **Light theme, `/theme`, and a `[ui]` config table.** The TUI now ships a
  light palette alongside the default dark one. `/theme dark|light` switches
  live and persists as `ui.theme`; every previously hardcoded widget color
  (role markers, diff backgrounds, modal text, the queued-message band) now
  routes through the theme.

- **`NO_COLOR` support.** Setting `NO_COLOR` (any non-empty value, per
  no-color.org) renders the whole TUI in the terminal's own default colors.
  Layout, glyphs, and bold/dim structure are unchanged.

- **Compose in `$EDITOR`.** Ctrl+O (or `/editor`) suspends the TUI, opens
  the current input draft in `$VISUAL`/`$EDITOR`, and loads the result back
  into the composer on save-quit. Works mid-run (it only edits the draft);
  recordings capture the returned text, so `--replay` never launches an
  editor.

- **`web_fetch` find-in-page.** New optional `pattern` argument: instead of
  returning the whole page, the tool returns each match (plain
  case-insensitive substring, per line) with `context_lines` of surrounding
  context (default 2), line-numbered and merged into blocks, capped at 20
  blocks with a `(+N more matches)` tail. Matching runs on the full page
  before the output cap, so tail matches on long pages are found.

- **Provider request ids in error messages.** Errors built from a provider's
  HTTP response now capture `x-request-id`/`request-id`/
  `anthropic-request-id` and `cf-ray` and append them as a
  `(request-id: ..., cf-ray: ...)` line to the user-facing message — quote
  it when reporting provider failures. Log output is unchanged.

- **Mid-run steering.** Messages typed while the agent is working are now
  delivered at the next tool boundary WITHIN the run — committed as user
  messages right after the tool results, so the very next model call sees
  them and course-corrects mid-task. Previously queued input waited for the
  whole run to end. Messages queued mid-stream with no later tool boundary
  still deliver at run end; images attached to queued messages ride along.

- **Checkpoints anchored to the conversation.** File checkpoints now record
  which session and message position produced them. Rewinding with
  double-Esc reports the checkpoints the discarded timeline left behind and
  names the one to `/restore` to roll files back to the fork point — rewind
  itself still never touches files.

- **Deferred MCP tools via `tool_search`.** MCP tools are no longer all
  advertised on every request: the model gets one `tool_search` tool that
  searches the deferred pool (names + descriptions) and promotes matches to
  direct advertisement for the rest of the session. Bounds the always-on
  tool surface (and the prompt tokens it costs) no matter how many servers
  are configured. Opt out with `mcp_defer_tools = false` globally or
  `defer = false` per server.

- **Concurrent, timeout-bounded MCP startup.** Servers now start in
  parallel, each with a 60s wall-clock bound, and report Ready/Errored
  individually as they resolve — one hung server no longer delays (or
  blocks) the rest. A timed-out server reports `startup timed out after
  60s` instead of hanging startup.

- **Provider-safe MCP tool names and schemas.** Tool names are sanitized at
  ingestion (charset `[A-Za-z0-9_-]`, 64-char cap with a stable hash suffix
  on overflow/collision — server names containing `__` no longer break
  routing) and input schemas are normalized for strict provider validators
  (local `$ref` inlined, `$defs`/`$schema` dropped, `const` to `enum`,
  nullable `anyOf` flattened, draft-4 boolean exclusive bounds dropped).
  `enabled_tools`/`disabled_tools` filters keep matching the raw names the
  server advertises.

- **Meta Muse Spark 1.1 now uses the Responses API with reasoning continuity.**
  `meta/muse-spark-1.1` authenticates with `MODEL_API_KEY`, streams text,
  automatic reasoning summaries, tool calls, usage, and multimodal inputs from
  `POST /v1/responses`. Mermaid uses Meta's stateless encrypted-replay mode
  (`store: false`) so reasoning survives tool turns and saved-session resume
  without server-managed response state; the opaque continuation is protected
  by existing private-session permissions and never rendered or written to
  diagnostic logs. Other providers keep their existing endpoints and behavior.
  One migration note: the per-message `thinking_signature` field became the
  provider-neutral `provider_continuation`, so Anthropic extended-thinking
  sessions saved before this change resume without their replayed thinking
  blocks (new turns re-establish them; no request errors).

- **Rewind and fork with double-Esc.** Pressing Esc twice within a second
  while idle opens a picker of the session's earlier user messages (newest
  first). Selecting one FORKS the session at that point: the original is
  saved and preserved (it keeps appearing in the `--resume` picker, now with
  its lineage recorded via `forked_from`/`parent_session`), a new session id
  takes over with the history before that message, and the composer is
  pre-filled with the message itself — its pasted images re-staged — so you
  edit and resend to branch the timeline. Busy Esc is unchanged (still
  cancels); a first idle Esc shows an `esc again to rewind` hint on the
  input box. The whole flow is key-driven and pure, so record/replay work
  unchanged.

- **@-mention fuzzy file picker.** Typing `@` at the start of a word in the
  composer opens a fuzzy picker over the project's files (ripgrep's
  gitignore-aware walker — hidden files, `.git`, and ignored paths excluded;
  capped at 20k entries) ranked by nucleo, the matcher Helix uses. Up/Down
  navigate, Tab or Enter inserts the relative path as plain text
  (`@src/foo.rs `) — the model reads it with its own tools, so mentions
  survive persistence, compaction, replay, and every provider. Esc dismisses
  for the current token; typing reopens. `user@host` never triggers, and the
  slash palette keeps `/` input.

- **`mermaid feedback` + an always-on TRACE ring.** A new in-memory ring
  captures the last ~2000 trace events from mermaid's crates at TRACE level
  (dependencies capped at INFO) regardless of `RUST_LOG`, with secrets
  redacted at capture — so a bug that already happened is diagnosable without
  a reproduce-under-logging round trip. `mermaid feedback` bundles the doctor
  report, a names-and-booleans config summary (provider keys reported as
  present/absent, never values), recent session ids, the trace ring, and the
  log tail into a local `mermaid-feedback-<ts>.md` (mode 0600; `--stdout` /
  `--format json` available). Nothing is uploaded; the rendered bundle passes
  a final whole-document redaction sweep. `RUST_LOG` now scopes only the file
  log layer.

- **Render snapshot suite.** Nine curated TUI scenes (idle, transcript,
  streaming, tool execution with a queued message, approval modal, question
  modal, conversation picker, slash palette, system notice + compaction
  checkpoint) are now pinned as full-frame `insta` snapshots at 80x24 and
  120x40, with a determinism self-check. Any unintended visual drift fails CI
  with a diff; deliberate changes are reviewed with `just snapshots` and the
  updated `.snap` files ride in the same PR.

- **Context windows and output caps are now discovered live, not hardcoded.**
  Anthropic and Gemini turns fetch the model's real limits from their models
  endpoints (`max_input_tokens`/`max_tokens`, `inputTokenLimit`/
  `outputTokenLimit`) — cache-first in `provider_probes` with the same 30-day
  TTL the Ollama probe uses, one same-host GET per (provider, model) on a
  miss. Claude turns now see their real 1M-token windows and 128K output
  ceilings instead of the rotted 200K/64K pins (auto-compaction was firing
  ~5x too early), the stale per-family output table is deleted outright, and
  `mermaid model-info` reports the discovered numbers with a `probed`
  confidence. GPT-5.6 (1.5M) joins the static catalog — OpenAI's `/v1/models`
  exposes no limits, so OpenAI stays static-but-corrected. Edge case: an
  Anthropic-compatible gateway id the Models API 404s on falls back to a
  conservative 8192 output floor on AUTO; set an explicit `max_tokens` to
  override.
- **Mermaid learns output caps from provider 400s and retries.** When a
  provider rejects a turn naming the model's real per-response ceiling
  (Ollama Cloud's `exceeds model's maximum output tokens (N)` — the
  minimax-m3 incident — or the OpenAI-style `max_tokens is too large`), the
  cap is persisted to the limits cache, the request is clamped, and the turn
  retries once instead of dying. Every later turn sizes below the learned cap
  up front. The parser is deliberately strict: context-limit wordings never
  match, so a window can't be mislearned as an output cap.

- **Skills: SKILL.md playbooks with progressive disclosure.** Mermaid now
  discovers task-specific playbooks at startup — project
  (`<git-root>/.mermaid/skills/<name>/SKILL.md`), user
  (`~/.config/mermaid/skills/`), and enabled plugins (the manifest's `skills`
  list, containment-checked like hooks) — and injects a compact index (name,
  one-line description, path) into the system prompt. Same-named skills dedupe
  with project > user > plugin precedence; the index caps at 64 skills / 8 KiB
  with an overflow line. The model activates a skill by reading its SKILL.md
  with the existing policy-gated `read_file`, so activation is visible in the
  transcript and idle skills cost no per-request tool schema. Headless runs
  and subagents load the same index; `mermaid doctor` reports the count.

- **Plugin hooks can now gate tool calls.** On `before_tool_use`, an enabled
  plugin hook may deny the call, rewrite its arguments, or inject context for
  the model's next request via a Claude Code-compatible JSON response on
  stdout (`permissionDecision` allow/deny/ask, the legacy `decision: block`
  shape, plus mermaid's `updatedInput` and `additionalContext` extensions);
  exiting with code 2 also denies, with stderr as the reason. First deny wins
  across plugins, the last rewrite wins, context strings concatenate, and a
  rewritten call is still vetted by the safety policy. Intent fails closed
  (explicit denials always deny) while infrastructure fails open (a hook that
  times out or prints garbage logs a warning and allows) — a buggy hook can't
  lock you out of every tool. All other events remain observe-only.

- **`mermaid run` is session-addressable.** Every headless run now surfaces
  its session id — a new `session_id` field on the ndjson `session_started`
  and `result` lines and the json result (protocol stays v1; the fields are
  additive), plus a `session: <id>` line on stderr for the text/markdown
  formats — and accepts `--resume <id>` / `--continue` to seed the run from a
  saved session, appending to the same session file so repeated resumes chain.
  Headless runs also stamp git branch/SHA/CLI-version provenance like the
  interactive path, a run that ends in a provider error still persists its
  session (the emitted id never dangles), and a script that names a missing
  session or asks for `--continue` with none saved gets a hard error instead
  of a silent fresh session.


- **The transcript now records what you answered in `ask_user_question`.**
  An answered question set renders as a `User answered the model's
  questions:` block with one `· question → answer` line per question
  (notes included), Claude-Code style, instead of a bare
  `ask_user_question()` header with only a duration. Dismissed or
  chat-about-this resolutions show their outcome text too.

- **`model_profiles` renamed to `[model_aliases]`** (and the `profile:`
  model-id prefix to `alias:`) to free the "profile" name for the config
  overlays above. Bare alias names still resolve unchanged.


- **Ctrl+C now requires a second press to exit.** The first Ctrl+C does the
  useful thing — interrupts a running turn (like Esc) or clears typed input —
  and shows "press ctrl+c again to exit" for 3 seconds; a second press inside
  the window exits. Ctrl+D on empty input and `/quit` still exit immediately.


- **Tool-call transcript labels distinguish creating a file from changing one.**
  A `write_file` that overwrites an existing file now reads `Update`, not
  `Write` — `Write` is reserved for a genuinely new file — and targeted
  `edit_file` calls read `Update` too. The vocabulary is now
  `Write` / `Update` / `Delete` (previously `Write` / `Edit` / `Delete`),
  matching Claude Code, so it's clear at a glance whether a call created,
  modified, or removed a file. The create-vs-modify distinction comes from the
  `created` flag the write tool already records, so it's accurate even when the
  model rewrites a whole file with `write_file` instead of `edit_file`.

- **File-diff summaries read as words, like Claude Code.** The `⎿` line under a
  `Write` / `Update` now reads `Added 49 lines, took 25ms` or
  `Added 7 lines, removed 1 line, took 25ms` (and `Removed 9 lines, took 25ms`),
  replacing the terse `Success, +49 -0, took 25ms`. Empty clauses are dropped and
  `line`/`lines` agree with the count; the timing is kept, the redundant
  `Success,` prefix removed.

- **`apply_patch` reads as `Update`, and `Success` is dropped from result lines.**
  An `apply_patch` call now shows `● Update(<file>)` — or `Write` / `Delete` by
  operation, `Update(N files)` for a multi-file patch — instead of the model's raw
  `Apply patch()` with empty parens, matching the `Write` / `Update` / `Delete`
  vocabulary. Separately, the redundant `Success` prefix is gone from every result
  line (a failure renders differently, so a plain success needs no label): e.g.
  `3 lines read, took 1.2s`, and a delete shows just `took 35ms`.

- **`mermaid list` and `mermaid models` no longer start Ollama.** All four
  read-only verbs (`list` / `models` / `status` / `doctor`) now enumerate
  with auto-start hard-off: observing state never mutates it, so a
  cloud-model user who deliberately stopped Ollama to free VRAM can run any
  of them without resurrecting the daemon. A dead server is reported
  honestly ("Ollama is installed but not running — local models can't be
  listed") instead of the misleading "No Ollama models installed locally."
  Auto-start remains on the paths that actually use Ollama: chat and the
  startup model check.

### Fixed

- **Word-wrap no longer styles or invents spaces at span boundaries.** On
  chat lines long enough to wrap, the wrapper re-joined words with a
  separator space styled like the following span — so the gap before a
  markdown link rendered underlined (and before inline code would carry the
  code background) — and treated every span boundary as a word boundary, so
  spans adjacent without source whitespace gained a phantom space
  (`(url) .` after a link, `` `code` , ``, `bold suffix` for
  `**bold**suffix`). Wrapping now flattens spans into a word stream where
  boundaries exist only at real whitespace, emits separator spaces
  unstyled, and hard-breaks over-long multi-style tokens while preserving
  each fragment's style.

- **Compaction preserves the complete live context and persists in order.**
  Summary requests now budget the entire request, include redacted tool
  arguments and referenced images, count cached and reasoning tokens, and
  require the documented summary structure before replacing history. Messages
  that arrive while compaction runs are retained, truncation recovery resets
  after visible progress, checkpoint metadata reports review status and actual
  preserved turns, and serialized archive-plus-conversation saves prevent a
  newer stripped transcript from bypassing a failed or delayed archive.
  A compaction save that arrives while an older archive write is still
  failing queues behind it instead of being dropped, shutdown drains every
  conversation's pending barrier, and quoted `## ` lines inside checkpoint
  bodies no longer fail structural validation. A failed auto-compaction now
  pauses further automatic attempts (with a one-time notice) until a
  compaction succeeds, `/compact` runs, or the conversation switches. A late
  tool result that lands during compaction is re-inserted after its pending
  call, and mid-compaction message matching uses compact sha256 fingerprints
  instead of cloning the full transcript into the result. Persisted
  compaction records and recorded compaction events changed shape
  (`review_status`/`preserved_turn_count` replace
  `verified`/`verification_error`; boundaries are now fingerprints), so
  session files with compaction records and session recordings with
  compaction events from earlier unreleased builds will not load or replay.

- **The status spinner never names tools; the transcript does.** The spinner
  headline used to splice in the executing tool and its arguments
  (`Running tools: Bash pwd; ls -la…`, `Running tools: ask_user_question...`)
  — detail that belongs in the chat window, not the status widget (Claude
  Code parity). The headline is now only the task's active form, the
  "Running N agents" override, or the bare phase word, followed by the usual
  `(esc to interrupt • time • tokens)` metadata. Instead, each executing
  tool call gets its `● Bash(cmd)` action row in the transcript the moment
  it starts — with a blinking header dot while it runs — and its result
  elbow folds in underneath as soon as that call completes, not only when
  the whole batch commits. Pending `agent` calls keep their live panel rows
  (no duplicate transcript row), and a pending `ask_user_question` is
  represented by its modal alone, with the question → answer block landing
  once answered.

- **The question modal owns the screen.** While an `ask_user_question` modal
  is up, the status spinner, task checklist band, and the input box (keys
  already routed exclusively to the modal) are hidden — matching Claude
  Code, where the modal is the only thing below the transcript instead of
  sitting under a ticking `Running tools: ask_user_question...` spinner and
  an inert prompt.

- **Task checklist no longer dangles a `⎿` connector when idle.** The elbow
  glyph on the checklist's first row exists to attach the band to the
  status/spinner line above it; once the run went idle and that widget
  disappeared, the elbow was left hanging from nothing. Idle now renders the
  expanded checklist flush-left with no connector, and a collapsed (Ctrl+T)
  checklist disappears entirely between runs — collapse is a
  minimize-while-working affordance, so with no status line there is nothing
  to minimize into. Active-run rendering is unchanged, and the collapsed
  state persists: the "Next:" one-liner reappears when the next run starts.

- **Tool action lines wrap instead of clipping at the viewport edge.** Long
  tool headers (`● Bash(python3 - << 'PY'…`), result summaries, error bodies
  (e.g. a full HTTP 404 JSON), diff rows, and write previews were painted as
  single over-wide rows and cut off at the right edge. They now wrap with a
  hanging indent: headers preserve a multi-line command's own line breaks
  (previously the newlines were dropped, gluing fragments together like
  `'PY'from PIL import`) and cap at 4 rows with a trailing `…)` so a heredoc
  script can't flood the transcript; results and errors wrap in full; diff
  rows keep their full-width color bar on every wrapped row.

- **Safety-mode switch note: fires once, model-only.** The "earlier read-only
  policy blocks no longer apply" note used to appear in the transcript on
  every loosening step (ask, then auto, then full_access — three banners for
  one Shift+Tab cycle), because the trigger was "any loosening while a
  read-only denial exists in history" and the denial text is never removed
  from stored history. The note now injects only when actually leaving
  read_only past a stale denial, is hidden from the transcript (the status
  bar already shows the mode; only the model sees it), and steers exactly
  one request before being swept. Cycling onward renames the one pending
  note instead of stacking new ones, and tightening back to read_only
  retracts it.


- **Existing `[model_profiles]` config tables migrate automatically.** The
  rename to `[model_aliases]` left older config files warning about an
  unknown key; loads now migrate the table in memory (warning gone
  immediately) and the next settings persist renames it on disk.


- **The status line no longer flickers raw subagent text.** Every child
  stream chunk used to overwrite the status line (garbage fragments flying
  past at stream speed); children now report only stable activity — their
  current tool, coarse phase changes, and a token count throttled to twice a
  second. The run token counter also keeps climbing during agent turns
  (children's live counts ride on top) instead of freezing until the tools
  return.

- **Token accounting normalized; one honest set of meters.** The footer
  showed three token counters measuring three different things: `context`
  and `last api` were the same number rendered twice, and `session` was a
  naive sum of every API call's full total — the re-sent conversation input
  (and cache reads) counted again on every call, so a short agentic run
  read "1.2M". The footer now shows only the context gauge
  (`context: 24.5k / 1M (2%)`); the cumulative sum lives in `/usage`,
  labeled as what it is (all API calls, subagents included, input/output/
  cache broken out). The run summary ("Worked for 6m · used N tokens") now
  counts real provider-reported output tokens across the run — parent
  turns, subagents, and mid-run compactions — falling back to the old
  chars/4 estimate (marked `~`) only when a provider reports no usage.
  Underneath, `TokenUsage` stores only disjoint components and derives all
  totals, fixing two real unit bugs: each adapter previously invented its
  own `total_tokens` (Anthropic's included cache reads, OpenAI's didn't),
  and OpenAI-compat/Meta double-counted reasoning tokens in output totals
  (wire `completion_tokens` already includes them) — OpenAI-visible output
  numbers shrink accordingly.
- **Rate-limit errors now say what actually happened.** A provider 429 used
  to surface as the inscrutable "retry after None"; it now shows the
  provider's own reason from the response body (e.g. Cloudflare's "you have
  used up your daily free allocation of 10,000 neurons") — the difference
  between "wait a moment" and "upgrade your plan" — plus the server's
  retry-after when sent. 429 retries also got their own backoff schedule
  (~2s then ~5s instead of the 5xx 500ms→1s): retrying inside the same rate
  bucket always lost. `Retry-After` headers were already honored and still
  win when present.
- **Cloudflare Workers AI models now report their real context window.**
  Cloudflare's OpenAI-compatible `/models` lists bare ids with no limit
  metadata, so the footer gauge read "context: … / unknown" and compaction
  sizing flew blind. Live limits discovery now queries the account's
  `models/search` endpoint instead (openrouter format first — context window
  plus output cap for marketplace models — falling back to the full-catalog
  default format for everything else). AI Gateway base-url overrides keep
  the previous generic behavior.
- **The full repaint no longer kills the TUI.** The post-shell-command
  repaint (and Ctrl+L) queried the terminal for the cursor position, and the
  reply raced the input reader thread — the first shell command the model
  ran could exit the app with "The cursor position could not be read within
  a normal duration". The repaint now clears and redraws without ever
  querying the terminal.
- **Headless runs no longer truncate auto-continued replies.** `mermaid run
  -p … --output text/markdown/json` returned only the final continuation
  segment; it now joins the whole chain, echo-trimmed.
- **Image clicks resolve by their stable `[Image #N]` number.** Ctrl+Click
  previously mapped through the display position, which the transcript
  stitch can shift; the global number now wins, with the positional pair as
  fallback for pre-numbering sessions.
- **A run that ended at the auto-continue cap no longer starts the next run
  with an empty continuation budget** (the counter now resets on submit,
  like its truncation/empty-turn siblings).
- **Shell commands can no longer grab the terminal.** Model-run commands now
  start in their own session (`setsid`) with no controlling terminal, so a
  child that opens `/dev/tty` — a `sudo` password prompt, an ssh passphrase
  read — fails instantly instead of painting its prompt over the TUI and
  hanging until the command timeout. Git is additionally told never to prompt
  (`GIT_TERMINAL_PROMPT=0`) in foreground and background commands alike.
  Esc-cancel, timeout tree-kill, and Ctrl+B backgrounding semantics are
  unchanged; no behavior change on Windows.
- **The TUI now recovers from stray writes to the terminal.** ratatui only
  repaints cells it knows changed, so bytes another process wrote directly to
  the tty (e.g. a child that opened `/dev/tty`) used to persist as ghost
  characters that typing couldn't dislodge. The screen now fully repaints
  after each shell command finishes, and Ctrl+L forces an immediate repaint
  at any time.
- **Ctrl+Shift+C no longer quits — it copies the selection.** Mermaid now
  negotiates the kitty keyboard protocol (disambiguated key encoding) where
  the terminal supports it, so Ctrl+Shift+C arrives distinct from Ctrl+C and
  triggers the existing drag-select copy instead of the quit path; Esc
  reporting also becomes unambiguous. On terminals without the protocol the
  two chords are physically identical on the wire — there the new press-twice
  behavior below keeps a stray copy attempt harmless.


- **Read-only shell commands prefixed with `cd` are no longer blocked.** A
  `cd DIR && <read>` command (e.g. `cd repo && git status`) classified as a
  mutation because `cd` wasn't a recognized read-only head, so read_only mode
  blocked the whole compound command. `cd`/`pushd`/`popd`/`dirs` (plus
  `base64`/`seq`) now classify as read-only, and the read-only git allowlist
  gained the pure-read subcommands `rev-list`/`merge-base`/`show-ref`/
  `for-each-ref`/`name-rev`/`show-branch`/`count-objects`/`version`. The
  worst-segment rule still catches any real mutation in a later segment.
- **The model no longer believes it's still in `read_only` after switching to a
  looser mode.** A mutation denied in read_only left a `blocked by policy:
  read-only safety mode …` tool-result in the conversation, which was re-sent
  every turn; after switching to `full_access` the model trusted those stale
  errors over the (correct, live) mode and kept refusing edits — or claimed the
  runtime was "still read-only." Superseded read-only denials are now rewritten
  to a past-tense note once the live mode is looser, and loosening the mode past
  such a denial surfaces a one-line confirmation.
- **Command, tool, and web output truncation keeps the tail.** Output past the
  cap is truncated in the middle (head + tail with an elision marker) instead of
  head-only, so a failing command's actual error — which lives at the end — is
  no longer discarded before the model sees it.
- **Sessions saved mid-tool-call resume cleanly.** `tool_use`/`tool_result`
  pairing is normalized before every request and when a session is loaded, so a
  session persisted while a tool was in flight no longer resumes into an
  unrecoverable provider 400.
- **OpenAI-compatible vision is reported truthfully.** `supports_vision` is now
  derived from the model id instead of hardcoded `false`, so `/doctor` and
  `/model` no longer under-report vision for capable models and the no-vision
  warning fires for genuinely text-only models.
- **The approval and question brokers recover from a poisoned lock instead of
  cascading panics.** They took `Mutex::lock().unwrap()`, so a panic while
  holding the (tiny, synchronous) lock would poison it and every subsequent
  access would panic in turn. They now recover the guard on poison, matching the
  pattern already used elsewhere in the codebase.
- **The streaming relay task can no longer leak on cancel.** Each provider's
  stream bridge spawned a relay task whose handle was dropped (not aborted) when
  a turn was cancelled; parked on a full downstream sink, it could outlive the
  turn. It now rides an abort-on-drop guard (promoted to a shared util), so a
  cancelled turn aborts it — the same structural task ownership the effect runner
  already used internally.
- **`temperature = 0.0` is now honored.** Setting an explicit `0.0`
  (deterministic / greedy decoding) was silently overridden with the `0.7`
  default — a leftover `> 0.0` guard treated a deliberate zero as "unset." It now
  passes through verbatim.
- **Calling a stopped MCP server now returns a clear error.** After
  `stop_server`, a later `call_tool` hit the dead server and surfaced a
  broken-pipe transport error instead of saying the server was stopped. The
  client is now flagged on shutdown and the manager returns a clean
  "MCP server '…' has been stopped" message.
- **The queued-message FIFO is now bounded.** Prompts typed while a turn is in
  flight are queued and auto-submitted when it finishes; holding Enter through a
  long turn could grow that queue without limit. It's now capped at 32, dropping
  the oldest with a warning.
- **A misbehaving Ollama stream can't exhaust memory.** The newline-delimited
  JSON reassembly buffer had no cap, so an endpoint that streamed bytes without
  ever sending a newline would grow it until OOM. It now enforces the same 8 MiB
  reassembly cap the SSE (Anthropic/Gemini/OpenAI) streams already had.
- **The config file is now written atomically.** `save_config` truncated the
  config in place, so a crash, kill, or disk-full mid-write could leave an empty
  or half-written `config.toml` — losing your settings (and inline secrets). It
  now writes via a temp file + fsync + atomic rename, created `0o600` on Unix so
  the secret-bearing config is never even briefly world-readable.
- **Failed background saves now report what went wrong instead of vanishing.**
  Conversation saves, the compaction archive's conversation write, model /
  reasoning / Ollama-preference persistence, the `--record` replay log, the
  daemon's TCP hint file, and the `ask_user_question` "remember this answer"
  store all dropped their error on failure (some logged a bare "failed" with no
  detail). Each now logs the underlying error, and the answer-prefs write is
  atomic so a crash mid-save can't truncate it.
- **Cancelling or resetting a turn no longer leaves a dead question modal on
  screen.** An `ask_user_question` prompt parked mid-turn survived `/load`,
  `/clear`, Ctrl+C, and quit — the tool task behind it was torn down, so the
  modal could never be answered. All four paths now clear it (and the stale
  running-tool indicator) alongside the pending approval they already dropped.
- **A context-limit compaction no longer silently drops your turn.** When a
  provider rejected a request mid-stream for length, Mermaid compacted the
  conversation but then ended the turn instead of resuming — abandoning the work
  you asked for. It now resumes the request after compacting, exactly like a
  truncation recovery. Relatedly, a system note posted *during* a compaction
  (e.g. an MCP server error) is now inserted before a pending tool call rather
  than wedged between a `tool_use` and its `tool_result`, which some providers
  reject on the next request.
- **The daemon reaps orphaned background-command logs on startup.** Ctrl+B-
  detached commands leave a tee log (capped at 64 MiB each) in the private temp
  dir; across many restarts with backgrounded processes these accumulated
  forever. The daemon's startup recovery now sweeps `mermaid-bg-*.log` files
  older than `[daemon] retention_days` (a live detached process keeps its log
  fresh, so an old mtime means the writer is long gone).
- **The daemon's `outcomes` and finished-`tasks` tables no longer grow without
  bound.** The startup GC now prunes terminal tasks (with their events) and the
  append-only `outcomes` reward table, which #148's durable queue would otherwise
  keep — with their full prompts — forever. `outcomes` (the self-improving-loop
  training corpus) is retained on its own, deliberately longer window so a large
  training history survives the shorter task/session retention, and each outcome
  is stamped with its task's context so it stays usable after that task is
  pruned. New `[daemon] retention_days` (default 30) and
  `[daemon] outcomes_retention_days` (default 180) tune the two windows.
- **A daemon task that produces an empty response is recorded as a failure, not
  a success.** A run that returned no error but also no text was mapped to
  `Completed` and stamped a `task_terminal` success/1.0 into the `outcomes`
  training corpus — a false positive the self-improving loop would learn from. It
  is now a `Failed` task with a clear report, so the reward signal reflects that
  nothing was produced.
- **Pasting an image and immediately pressing Enter no longer drops the image.**
  Ctrl+V reads the clipboard asynchronously, so a fast paste-then-Enter could
  submit the message before the image arrived — sending it with no image (and
  leaking a stray `[Image #N]` into the next prompt). Enter now waits for any
  in-flight clipboard read to land, then submits with the image included. The
  read result rides a dedicated internal message so an empty or failed read
  still releases the held submit instead of wedging it, and a normal terminal
  paste is never mistaken for a Ctrl+V read.

### Security

- **The debug log and conversation store are owner-only and redacted.**
  `~/.mermaid/mermaid.log` and the saved conversation/compaction transcripts are
  now written `0600` and scrubbed of credential-shaped strings, so a `read_file`
  of `.env` or an API error echoing a key can't sit in cleartext in a
  world-readable file.
- **Concurrent same-path file edits are serialized.** A per-path async lock
  prevents two file-mutating tool calls in one turn from silently clobbering
  each other (last-writer-wins) or racing a read-modify-write.
- **`web_fetch` now pins DNS resolution against rebinding.** The native fetch
  resolved a URL's host and vetted the addresses, then let the HTTP client
  re-resolve at connect time — a TOCTOU a hostile DNS server could exploit to
  pass the pre-check with a public IP, then connect to `127.0.0.1` /
  `169.254.169.254`. A custom resolver now vets the resolved addresses at connect
  time, for the initial host and every redirect hop, failing closed on any
  internal address — so the connection binds to exactly what was vetted.
- **`open_url` (background-mode browser launch) now validates the URL scheme and
  no longer routes through a shell on Windows.** The model-supplied `open_url`
  was passed unvalidated — and on Windows via `cmd /C start`, so shell
  metacharacters (`& | > ^`) in the URL could execute arbitrary commands, while a
  `file:` / `javascript:` URL could reach the desktop handler on any OS. It is
  now rejected unless it is `http`/`https`, and launched on Windows via
  `rundll32` (a real executable, single argv — no shell re-parse). Loopback URLs
  stay allowed so opening a just-started local dev server still works.

### Infrastructure

- **Repo hygiene: `AGENTS.md`, a `justfile`, CI source guards, nextest, and
  CHANGELOG-driven releases.** A root `AGENTS.md` encodes the real invariants
  (MVU purity, no emojis, no back-compat shims) and a `justfile` provides the
  one-command pre-PR gate (`just check`). CI gained two dependency-free guards —
  no emoji/pictographs in source, and `src/domain` stays a pure MVU core (no
  I/O, no wall clock) — plus a daemon-integration + `self-test` job, and switched
  the test runner to `cargo-nextest` (retries auto-heal the Windows cancellation
  flake). Releases now build their notes from the curated CHANGELOG section
  (failing if the tag has none) and smoke-test each built binary (`version` +
  `self-test`) and the published install script.

- **Render snapshots survive release bumps.** The status footer's version
  string now threads through `RenderCache` (like the pinned hostname and
  username) instead of being read from `env!("CARGO_PKG_VERSION")` at render
  time, and the snapshot suite pins it — so version bumps no longer invalidate
  every pinned TUI frame.

## [0.16.0] - 2026-07-04

### Added

- **Mermaid now starts Ollama itself.** When a request to a *local* (loopback)
  Ollama URL is refused — cold boot after a reboot, server crashed mid-session
  — mermaid finds the `ollama` binary (PATH or the platform's default install
  locations), launches `ollama serve` detached (it survives mermaid exiting
  and ignores the TUI's Ctrl+C), waits for it to come up, and retries the
  request. No more leaving mermaid to run `ollama serve` by hand. Applies to
  chat, `mermaid models`/`list`, and the startup model check, for local and
  `:cloud` models alike; remote hosts are never touched, and the diagnostics
  (`mermaid status` / `doctor`) deliberately observe without healing — they
  now report "installed but not running" instead of conflating it with "no
  models". "Is Ollama installed" checks share the autostart's binary
  discovery, so a fresh install whose PATH hasn't reached the current shell
  is still found. If auto-start can't help (e.g. Ollama isn't installed), the
  connection error says exactly that and where to get it. Opt out with
  `auto_start = false` under `[ollama]`, or `MERMAID_OLLAMA_AUTOSTART=0` in
  the environment (containers/CI).

- **`mermaidd` now runs on Windows.** The daemon serves its JSONL control
  protocol over a named pipe (`\\.\pipe\mermaidd-<user-SID>`) locked to the
  owning user with an explicit security descriptor — the named-pipe analog of
  the `0600` Unix socket + peer-uid check — with remote pipe clients rejected
  and the first pipe instance doubling as the single-daemon guard. The
  `mermaid` CLI and the optional localhost TCP listener work unchanged.
  Service install (`mermaid daemon install`) remains systemd/Linux-only; on
  Windows start `mermaidd.exe` manually or via Task Scheduler.

- The runtime store now records **outcomes** — a durable, append-only table of
  verifiable results and reward/preference signals attached to a task (and
  optionally a specific tool run). Each outcome carries a `kind` (e.g.
  `task_terminal`, `test`, `preference`), a graded `label`, an optional scalar
  `reward`, and a `source` marking provenance (`verifier`/`user`/`model`/
  `system`) — the enrichment that turns the trajectory log into training data.
  `mermaidd` records a `task_terminal` outcome when a daemon-run task finishes.

## [0.15.1] - 2026-07-02

### Added

- The `--resume` picker can now delete a session: press `Del` on a row to
  remove its saved conversation (with a `y`/N confirm).

### Fixed

- Empty sessions are no longer saved: running `mermaid` and closing it without
  sending anything leaves no conversation file, so it can't clutter the
  `--resume` picker or be reached by `--continue`. Pre-existing empty session
  files are also filtered out of both resume paths.
- The `--resume` picker now scrolls properly: the mouse wheel scrolls the
  viewport (it previously moved the selection, because the alternate screen was
  translating the wheel into arrow-key sequences), and the list follows the
  selection when you arrow past the bottom or top instead of clipping.
- `--resume`/`--continue` now restore the full session state, not just the
  transcript: the safety mode and the token/context meters (the `context: …`
  and `session: …` figures in the status bar) are saved with the conversation
  and hydrated on resume, instead of resetting to `n/a` / `0` / the
  config-default safety mode. Safety-mode changes (`Shift+Tab`, `/safety`) now
  persist immediately; conversations saved before this fall back to defaults.

## [0.15.0] - 2026-07-02

### Added

- Web search and fetch now work out of the box with **zero configuration**, and
  are backend-pluggable via `[web]` config. `web_fetch` defaults to a **native**
  in-process backend (fetch the URL directly, convert HTML to markdown) — no
  key, no third party. `web_search` defaults to **`auto`**: Ollama Cloud when
  `OLLAMA_API_KEY` is set, otherwise mermaid **auto-starts and manages a local
  SearXNG container** (via podman/docker) on the first search and tears it down
  when it exits — you install and configure nothing. The first search pulls the
  SearXNG image once. Force a backend with `fetch_backend = "ollama"` or
  `search_backend = "ollama"`/`"searxng"` (your own instance at `searxng_url`,
  which must have the JSON format enabled).
- `mermaid --resume` opens a searchable picker of this directory's past
  conversations, styled like the main TUI (type to filter; each row shows the
  title and a `relative-time · branch · size` meta line). It replaces the old
  bordered `--sessions` picker (renamed for Claude Code parity). `--continue`
  is unchanged — it reopens the most recent conversation in the directory.
- Conversations now record the git branch they were worked on (shown in the
  `--resume` picker). Sessions saved before this backfill their branch on the
  next save; non-git directories simply omit it.
- Agent types for the `agent` tool. Built-in `general` (full tool access at
  your safety mode) and `explore` (read-only reconnaissance: reads +
  read-only commands, cannot mutate regardless of the parent's mode), plus
  user-defined types under `[agents.types]` in config — each a tool filter, a
  safety ceiling (the child runs at the *less* permissive of the parent's
  live mode and the ceiling, so a type can only tighten), a system-prompt
  preamble, and an optional default model. A custom name shadows a built-in,
  so `[agents.types.explore]` retunes the built-in. Pick a type with the
  tool's new `type` arg.
- Per-call subagent model override: the `agent` tool's new `model` arg (and a
  type's `model` default) runs a child on a different model than the parent —
  e.g. a cheap/fast model for search-and-summarize fan-out, the session model
  for synthesis. Priority: per-call `model` > type default > session model.
- Subagent continuation handles. Every `agent` result ends with an
  `[agent_id: …]` trailer; passing that id back as the new `agent_id` arg
  restores the child's conversation context and seeds the prompt as its next
  message, so a follow-up reuses what the child already learned instead of
  re-exploring. The most recent children (bounded cache) are retained;
  timed-out and errored children are kept too, so "continue aN: what did you
  find so far?" works.
- Configurable subagent timeout via `[agents] timeout_secs` (default 1200 =
  20 minutes), replacing the previously hard-coded ceiling.
- Live subagent visibility: while an `agent` call runs, the status line now
  shows the child's current activity ("Running tools: Agent explore crates ·
  read_file…") instead of an opaque spinner — the child's tool starts/finishes
  and latest text were already streamed to the parent but silently dropped by
  the reducer. Completed subagent rows also report what the child cost and
  which model ran it ("Success, 12.3k tokens · ollama/…, took 62s").
- Subagent report contract: a child session's system prompt now states that
  its final message is returned verbatim to the parent as the tool result and
  that nobody can answer questions — so children end with self-contained
  reports instead of "Want me to continue?".
- Subagents can now actually use MCP tools: the child's server entries are
  seeded Ready from the process-global MCP manager (shared with the parent —
  no per-child server processes), so `mcp__` tools are advertised to the
  child. Previously the registry carried the proxy but the tools were never
  advertised, making the documented capability dead in practice.

### Changed

- `web_fetch` now defaults to the native in-process backend instead of Ollama
  Cloud, so it works with no API key. Set `[web] fetch_backend = "ollama"` to
  keep the previous server-side behavior.
- The `--sessions` flag is renamed to `--resume` to match `claude --resume`.
  No deprecation alias — mermaid has no released users yet.

### Fixed

- Weak models no longer hit `unknown tool: web_search` when the web tools
  aren't configured. The system prompt now tells the model to call only tools
  present in its actual tool list, and the Ollama adapter no longer strips
  registered web tools by `OLLAMA_API_KEY` presence — which would otherwise
  have hidden the new keyless native `web_fetch`.
- A completing subagent no longer kills the parent's MCP servers: the child
  `EffectRunner`'s shutdown reaped the process-global MCP manager, so the
  first subagent to finish terminated every MCP server for the rest of the
  session. Child runners now leave the shared manager alone; only the
  top-level runner reaps it on exit.
- Subagent token usage now counts: the child session's provider usage rolls
  up into the parent's session totals and the end-of-run "used N tokens"
  summary (it was silently excluded — invisible spend on paid APIs).
- The system prompt advertised a nonexistent `subagent` tool; the registered
  tool is `agent`. It also now notes that subagent fan-out works in
  `read_only` (children inherit read-only), so models explore in parallel
  instead of assuming the spawn is blocked.
- `read_only` no longer blocks spawning subagents (user-reported): the
  `agent` tool now spawns in every safety mode, because the child inherits
  the parent's live safety mode and each child tool call is re-gated
  individually — a `read_only` child can fan out parallel exploration but
  still can't mutate anything. Operator `Deny` overrides on the subagent
  category/tool and the destructive-prompt hard-deny still block the spawn.

## [0.14.2] - 2026-07-02

### Fixed

- `read_only` no longer blanket-blocks `awk` (user-reported): the ubiquitous
  read-only idioms (`awk '{print $1}'`, field/pattern extraction, `-F`/`-v`)
  now classify as reads, so a pipeline like `… | awk -F/ '{print $1}' | sort`
  runs. `awk` that writes a file (`print > f`), runs a command (`system()`,
  `| "cmd"`), edits in place (gawk `-i inplace`), or loads an external program
  (`-f script.awk`) still classifies as a mutation and stays gated. (A bare
  `>` comparison like `awk '$1 > 5'` is conservatively treated as a write —
  indistinguishable from a redirect without a full awk parser.)

## [0.14.1] - 2026-07-02

### Security

- Closed a shell-classifier bypass found in a full audit: `yq -i` /
  `--inplace` rewrites a file in place but was rated read-only by its
  command name, so it auto-ran in `read_only` and `auto` modes. It (and
  `date -s`/`--set`, which sets the system clock) now classify as mutations.
  Their read-only invocations (`yq . f`, `date`, `date -d …`) are unaffected.

### Fixed

- `read_only` mode no longer blocks genuinely read-only commands
  (user-reported): redirects to the null-device family (`2>/dev/null` and
  friends) count as reads instead of writes; a glued separator
  (`ls 2>/dev/null; echo done`) no longer hard-denies the whole chain as a
  "sensitive `/dev/` write"; and `command -v NAME` — the POSIX binary-exists
  test, which executes nothing — classifies as the lookup it is. Redirects
  to real files, real devices (`/dev/sda`), and sensitive paths (`/etc/…`,
  `~/.ssh/…`) stay blocked, with regression tests pinning both directions.
- The read-only command allowlist gained the common pure-read tools it was
  missing, so they stop needing approval / stop being blocked in
  `read_only`: process/system inspection (`ps`, `groups`, `nproc`, `uptime`,
  `free`, `tty`, `arch`, `vmstat`, `ls{cpu,blk,usb,pci}`), binary/file
  inspection (`xxd`, `od`, `hexdump`, `strings`, `nm`, `objdump`, `readelf`,
  `size`), text tools (`nl`, `tac`, `rev`, `comm`, `join`, `paste`, `fold`,
  `fmt`, `expand`, `unexpand`, `[`), and the remaining checksum families
  (`b2sum`, `sha224/384/512sum`). Tools that can mutate (`strip`, `ldd`,
  `sed`, `awk`) were deliberately left off.

## [0.14.0] - 2026-07-02

### Security

- Daemon: the legacy plaintext socket commands were removed — every mutating
  command now goes through the token-gated JSON surface, so a local process
  can no longer bypass pairing.
- The safety gate's "don't ask again" allowlist no longer matches a command
  that contains a command substitution (an allowlisted prefix can't smuggle a
  `$(…)` payload), and shadow-git checkpoint snapshots skip absolute and `..`
  manifest entries — a crafted entry could previously truncate the very file
  being checkpointed via a self-copy.

### Added

- **`--replay <file>` — deterministic session replay.** A `--record` log now
  replays back through the pure reducer: `mermaid --replay session.jsonl`
  reconstructs the session headless (no model calls, no tool execution, no
  config reads — the log embeds its own config snapshot) and prints the
  transcript plus a determinism verdict. Every replay folds the log twice and
  exits non-zero if the folds diverge, making it a standing canary for
  reducer purity bugs.
- Recording format v1: recordings now start with a self-contained session
  header (config, model, cwd, `--continue` seed) and store every reducer
  input as a full serde round-trip — pasted images and tool artifacts ride
  as base64 and replay bit-exactly. Older (headerless, lossy) recordings are
  not readable; re-record with this version.
- Replay verifies against the live session, not just itself: recordings are
  sealed on clean exit with a fingerprint of the final session state, and
  `--replay` reports whether its fold reproduces the recorded outcome
  (`live match: yes / no / unknown`).
- Recordings no longer store the 60 Hz `Tick` stream (a documented reducer
  no-op, pinned by test) — hours-long recordings shrink from megabytes of
  ticks to just the meaningful inputs, with zero replay fidelity loss.

### Changed

- The reducer is now fully clock-pure: conversation mutations (message
  commits, compaction records, `/clear`'s fresh conversation id) derive
  every timestamp from the injected per-tick clock instead of reading the
  wall clock mid-update. Same recorded log in, same state out — the property
  `--replay` verifies and `tests/replay_determinism.rs` pins in CI.
- CI now builds, lints, and tests the full workspace — the runtime crate's
  test suite (daemon storage, checkpoints, policy, plugins) was silently
  excluded before.
- Dead-code sweep: the unused status-banner subsystem and a set of orphaned
  helpers/wrappers were removed (−544 lines), and the three divergent
  compact-count formatters were unified into one.

### Fixed

- **Cancelling (or quitting) mid-tool-execution no longer poisons the next
  turn.** Orphaned tool calls are sealed with cancelled placeholders, so the
  follow-up request can't be rejected for a dangling tool call; a message
  queued mid-turn no longer leaks across `/load` or `/clear` into the wrong
  conversation; and a mid-turn system notice can no longer split an
  assistant's tool call from its result (another next-turn rejection).
- **Headless runs finally see your project.** `mermaid run` and daemon tasks
  now load `AGENTS.md`/`MERMAID.md` project instructions and durable memory,
  matching interactive sessions; subagents load them synchronously instead of
  racing their first model call.
- OpenAI-compatible providers: assistant tool calls are wire-conformant
  (typed `function`, stringified arguments) — strict endpoints no longer
  reject the second turn — and image attachments are actually sent to
  vision models.
- MCP: a hung server can no longer wedge a turn (tool calls time out after
  5 minutes), and servers with paginated tool lists advertise all of their
  tools instead of just the first page.
- Repeated OS signals are all handled — previously the SIGINT/SIGTERM/SIGHUP
  handlers fired once and went quiet, so a second Ctrl+C from outside the
  TUI did nothing.
- Daemon: the accept loop survives transient connection errors instead of
  exiting, idle connections time out, and plugin hooks can no longer
  deadlock on large stdin payloads.
- Wide (CJK) characters no longer overflow truncated status lines, and
  concurrent config saves can no longer interleave and corrupt the file.
- A draw error during shutdown no longer skips MCP child cleanup and
  pending session saves.
- Release pipeline: the publish workflow verifies the tag matches the crate
  version, changelog extraction works from shallow checkouts, and the
  packaged systemd unit is generated from the same source as
  `mermaid daemon install` (with a drift-guard test).
- Clipboard operations can no longer hang Mermaid. Every clipboard subprocess
  (`wl-paste`/`wl-copy`, `xclip`, `pbpaste`/`pbcopy`, `osascript`, PowerShell)
  now runs under a kill-on-timeout deadline, so a frozen selection owner or a
  stale display connection surfaces as a visible paste/copy error within
  seconds — instead of a paste that silently never lands, a permanently leaked
  blocking thread, and a stuck child process that could stall shutdown.

## [0.13.0] - 2026-06-30

### Security

- **Fixed a critical sandbox bypass.** A destructive command hidden inside a
  command substitution (`$(…)` / backticks / `<(…)`), or obfuscated with `${IFS}`
  word-splitting or interior `..`, could be classified as read-only and auto-run
  with no approval in `read_only`, `ask`, and `auto` modes. The policy gate now
  recurses into substitutions and normalizes these forms, and fails safe when a
  command is nested too deep to fully analyze — so a hidden `rm -rf /` can no
  longer ride a benign-looking outer command. The gate is shell-aware end to end,
  so flag reordering, glued operators, and quoting can't downgrade a command's risk.
- Approval replay is confined through the same symlink-safe path checks
  (`openat2`) as the live path, and re-verifies a command isn't destructive before
  re-running it.
- Secrets are redacted more thoroughly (key-name-aware, more token formats), the
  config file is written `0600`, MCP child processes start from a clean
  environment, and terminal escape sequences in tool output are neutralized.
- MCP: package names are validated (no argument injection via a leading dash),
  and a provider `base_url` override that would send your API key to a
  non-loopback host must use HTTPS and warns you which host will receive the key.

### Fixed

- **A stalled turn no longer ends the run silently.** When the model spends a turn
  "thinking" but produces no reply and no actions, Mermaid auto-retries the
  request once (nudging the model) instead of leaving you at a finished timer with
  no output; if it's still empty, you get a clear hint instead of silence.
- An abnormally-closed model stream is surfaced as an error instead of being
  mistaken for a complete (empty) response — across all providers.
- Project instructions: `MERMAID.md` keeps its precedence even when the combined
  `AGENTS.md` + `MERMAID.md` exceed the size cap, a single unreadable instruction
  file no longer drops the others, and Windows home-directory resolution is fixed.
- Checkpoint restore is memory-bounded (one file at a time) and rollback is
  crash-safe — a failed restore can be rolled back in full, including non-empty
  directory subtrees.
- Assorted robustness fixes: idempotent daemon fallbacks, ownership-scoped task
  reconciliation (won't clobber a live session's task), deterministic MCP tool
  ordering, and per-model provider capability handling for current models.

## [0.12.2] - 2026-06-29

### Added

- A full-width gray highlight band behind your submitted prompts (Claude-Code
  style), keeping the `>` marker, so your messages stand out in the transcript.
- An end-of-run indicator: when an agentic run finishes, a dim "Worked for {time}
  · used {N} tokens" line appears where the spinner was — so a completed run has
  closure and you can see how long it took. It's display-only (never sent back to
  the model).

### Removed

- The chat transcript scrollbar. The transcript now spans the full pane width
  (the reserved right-hand gutter column is reclaimed); scrolling is unchanged.
- The per-turn "Reasoning hidden" placeholder line. With reasoning hidden (the
  default), turns now collapse silently instead of printing a
  `Reasoning hidden (/visible-reasoning on to show)` notice on every reasoning
  turn. `/visible-reasoning on` still reveals the thinking.

### Fixed

- **Markdown tables now render aligned instead of mangled.** Table lines are
  flagged preformatted so they're no longer word-wrapped (which collapsed their
  column padding), and tables wider than the terminal size their columns to fit
  and wrap long cell text within the column — nothing is lost and no row overflows.
- **Cloud (`:cloud`) Ollama models now use their full context window.** They run on
  Ollama's servers, not your local GPU, so Mermaid no longer VRAM-clamps them —
  e.g. `minimax-m3:cloud` uses its full ~524k-token window instead of being
  auto-fit down to your GPU (which it never touches). An explicit `/context <n>`
  still caps it if you want.
- Manual `/compact` on a conversation with too little history to summarize now
  shows a calm "Nothing to compact" note instead of a misleading
  "Compaction failed: Invalid request" error. Genuine compaction failures still
  report as failures.

## [0.12.1] - 2026-06-29

### Added

- **Automatic Ollama context sizing — you never touch Ollama config.** Mermaid
  probes an Ollama model's real context window + architecture dimensions
  (`/api/show`, cached in `provider_probes`) and auto-fits `num_ctx` to your GPU's
  VRAM so the model stays on the GPU. CPU/RAM offload is 5–20× slower, so it's off
  by default; the new `[ollama]` config keys `allow_ram_offload` (default `false`)
  and `max_auto_num_ctx` tune this. The status bar and `mermaid model-info` now
  report the real window instead of "unknown", and auto-compaction works for
  Ollama for the first time (it was silently disabled when the context limit was
  unknown).
- **Ollama context auto-converges to the real GPU fit.** Auto-fit is an estimate,
  so Mermaid now checks where the model actually loaded after each turn
  (`/api/ps`). If it spilled into CPU/RAM while offload is off, Mermaid shrinks
  `num_ctx` to the largest window that clears the measured overflow and reloads at
  it next turn, repeating until the model is fully resident on the GPU — or warning
  you once when even the minimum window can't fit (e.g. the weights alone exceed
  your free VRAM). `/context` reports the fitted window as `auto (GPU-fit)`, and
  `/context <n>` / `/context offload on` still override it.
- **Mermaid auto-compacts and continues when the context window fills.** On a small
  window (e.g. a local model auto-fit to a modest GPU), a response that hit the
  window mid-turn used to stop with a hint. Mermaid now compacts the conversation
  and resumes the run automatically, bounded by a per-run cap that resets whenever
  the run makes progress (so it only ever stops genuine no-progress thrashing). A
  new `[compaction]` config key, `max_truncation_recoveries`, tunes the cap
  (default `3`; `0` = uncapped).

### Changed

- Removed emojis from all user-facing output; status messages, warnings, and
  indicators now use plain-text markers.

### Fixed

- **Ollama responses no longer truncate early.** `max_tokens` is now forwarded to
  Ollama as `num_predict` (plus reasoning-aware headroom), bounded by the context
  window. Previously it was never sent, so a reasoning model would stop only when
  the tiny default window filled (`done_reason=length`).
- **The live token counter and spinner track the whole run.** The counter no longer
  sits at `0` during the thinking phase — it climbs as tokens stream — and the
  spinner plus its elapsed/token counters now persist across every tool step of an
  agentic run instead of resetting at each model call, so a long multi-step run
  shows one continuous, growing total.
- **Wrapped Markdown keeps its left margin.** Long assistant paragraphs no longer
  flush to column 0 when they wrap, and a wrapped bullet or numbered list item now
  hangs under its marker text instead of snapping back to the message gutter.

## [0.12.0] - 2026-06-28

### Security

- **Daemon, checkpoint, and storage hardening (review axis 3).**
  - Approval replay is now single-shot — a *denied* approval can no longer be
    resurrected as approved, and a stored action can't be replayed N times.
  - `restore_checkpoint` confines every restored path to the checkpoint's
    recorded project root; a tampered manifest can no longer write or delete
    files outside it (absolute paths and `..` escapes are rejected). The
    approval-replay exec path gets the same containment.
  - Pairing tokens are matched in constant time (no SQL `=` timing channel); the
    unauthenticated `pairings` socket command that exposed token hashes is
    removed; `logs` now requires the pairing token; daemon snapshots redact token
    hashes.
  - On Windows the data dir (SQLite DB with token hashes + transcripts) is locked
    to the current user via `icacls` instead of inheriting default ACLs.
  - Checkpoint shadow-git commands run with hooks disabled, and checkpoint /
    plugin manifests are written atomically.

### Fixed

- **Headless `mermaid run` output is no longer corrupted by a subagent.** A
  subagent's runner emitted an OSC 2 terminal-title escape (`\x1b]2;…`) into
  stdout even in headless mode (it didn't inherit the parent's title
  suppression), producing invalid `--format json` — and stray bytes in
  `text`/`markdown` — whenever the `agent` tool ran.
- **`mermaid run ""` now errors instead of silently doing nothing.** An empty or
  whitespace-only prompt is rejected at parse time (`prompt cannot be empty`,
  exit 2) rather than producing no output with a success exit code.
- **`mermaidd` no longer starts (or clobbers) the daemon when probed.** It
  ignored all arguments and went straight to binding the control socket —
  removing any existing one first — so `mermaidd --version`/`--help`/a typo would
  boot a foreground daemon, and doing so while the managed daemon was running
  would unlink its socket and orphan it. `mermaidd` now answers
  `--version`/`--help`, rejects unknown arguments (exit 2), and refuses to start
  when a live daemon already holds the socket (only a stale socket is removed).
- **Provider-adapter correctness (review axis 4).**
  - Truncation (`max_tokens`) and content-filter / safety refusals are no longer
    silently treated as a clean finish: a `⚠ truncated` note now appears, and a
    refusal that produced no usable content ends the turn with a clear error
    (Gemini's streaming path now matches its non-streaming behavior, applied
    across all adapters).
  - Anthropic streams cut mid-message (a proxy `Connection: close` without
    `message_stop`) no longer drop a fully-streamed tool call.
  - 429s now honor the server's `Retry-After` (capped at 60s) instead of a fixed
    ~1.5s backoff, surface as a typed rate-limit error, and every retry backoff
    is jittered to avoid synchronized retries.
  - OpenAI cached input tokens are no longer double-counted in the input total.
  - OpenAI-compat non-streaming responses strip inline `<think>` tags; the
    temperature is clamped to 0–2 for OpenAI-compat and Ollama.
- **Concurrency, runtime & MCP hardening (review axis 5).**
  - A slow or hung **plugin hook no longer freezes the app**: hooks now run off
    the event loop (`spawn_blocking`) and are killed if they overrun a 30s
    bound, instead of a synchronous `child.wait()` with no timeout.
  - **MCP servers are now gracefully shut down on exit** (stdin-EOF → terminate →
    kill ladder) instead of being orphaned, and `/mcp` stop actually kills the
    server's child rather than only updating the UI.
  - A flaky MCP server no longer slowly leaks request slots — the pending-request
    map entry is removed on timeout/error.
  - A cancelled foreground command now tree-kills its process group, so a
    grandchild it forked (`sh -c "server &"`) isn't orphaned.
  - Restarting a managed process waits (bounded) for the old PID to exit before
    respawning, avoiding a port clash with its predecessor.

### Changed

- **BREAKING — pairing tokens now expire.** New tokens default to a 30-day TTL.
  `mermaid pair` becomes `mermaid pair create [--label L] [--ttl-days N]`
  (`--ttl-days 0` = never expires), plus `mermaid pair list` and `mermaid pair
  revoke <id>`. Existing tokens get a 30-day grace window from first upgrade.
- **BREAKING — plugins install disabled.** `mermaid plugin install` no longer
  auto-enables a plugin; run `mermaid plugin enable <id>` (which now prints the
  plugin's declared capabilities) to activate its hooks. The manifest
  `permissions` field is renamed `capabilities` and documented as advisory
  disclosure, not a sandbox.
- **Provider `base_url` now requires HTTPS for non-local hosts.** A custom or
  overridden provider endpoint on plain `http://` to a public host is refused (it
  would send the API key in cleartext); `http://localhost` and private hosts stay
  allowed for local model servers (Ollama, vLLM).

## [0.11.1] - 2026-06-23

### Fixed

- **Status line no longer bleeds off-screen.** A long `Running tools: <cmd> …
  (esc to interrupt …)` now splits onto two rows when it doesn't fit and
  truncates each row to the terminal width — nothing overflows, including
  unbreakable file paths. The reserved height is stable and capped so the input
  box can't be evicted on a short terminal.
- **Esc never exits.** A second Esc while a turn was already cancelling used to
  quit mermaid (and could leave a backgrounded process holding the terminal). Esc
  now only cancels; only Ctrl+C / Ctrl+D / `/quit` exit.
- **Diff backgrounds fill the whole row.** Tab-indented diffs no longer show a
  ragged "staircase" — tabs are expanded so the red/green bar spans the full
  width, and tab indentation is now visible.
- **Quieter tool execution.** Live tool output (build lines, pids, streamed file
  contents) no longer flickers a transient line above the input; the status line
  names the running tool and full output stays in the transcript.
- **Ollama cloud models work on first use.** `mermaid --model <name>:cloud` no
  longer fails at startup trying to `ollama pull` a cloud model — cloud models
  are served by the daemon and skip the local pull.
- **Markdown loose-list bodies hang-indent** under their bullet instead of
  dropping flush to the left margin.
- **Installer takes PATH precedence** and warns when another `mermaid` (e.g. a
  stale `cargo install`) earlier on PATH would shadow the install.

### Added

- **Homebrew + Scoop + WinGet.** `brew install noahsabaj/mermaid/mermaid`,
  `scoop install mermaid`, and `winget install NoahSabaj.Mermaid` (once accepted
  upstream). All three are bumped automatically by the release pipeline.

## [0.11.0] - 2026-06-22

### Added

- **Install without cargo.** One-line installers download a prebuilt binary for
  your platform from the latest GitHub Release, verify it against `SHA256SUMS`,
  and put `mermaid` + `mermaidd` on your PATH — no Rust toolchain needed:
  - macOS/Linux: `curl -fsSL https://noahsabaj.github.io/mermaid-cli/install.sh | sh`
  - Windows: `irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex`

  Honor `MERMAID_VERSION` (pin a release), `MERMAID_INSTALL_DIR`, and
  `MERMAID_NO_MODIFY_PATH`. The scripts are served from GitHub Pages and stay
  canonical in the repo.
- **`mermaid update`.** Checks GitHub Releases for a newer version and updates
  in place by re-running the platform install script (`--check` to only report,
  `--force` to reinstall). Reuses the existing HTTP client — no new
  dependencies. On Windows the installer renames the running `mermaid.exe` aside
  so an in-place update succeeds.

## [0.10.2] - 2026-06-22

### Added

- **Background processes on Windows.** `execute_command` `mode="background"`
  previously errored with "not supported on Windows yet"; it now works — the
  command is spawned detached (`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`)
  with output redirected to a log file, and `/processes`, `/logs <id>`, and
  `/stop <id>` function (process liveness via `tasklist`, stop via
  `taskkill /T /F` tree-kill).
- **Ctrl+B sends a running foreground command to the background.** While an
  `execute_command` runs in the foreground, press `Ctrl+B` to detach it — it
  keeps running as a `/processes` entry, tail-able via `/logs`, instead of
  blocking the turn (the "oops, I ran the dev server in foreground" rescue, as
  in Claude Code). Its output is teed to a log file so it stays viewable. The
  status line advertises the shortcut while tools run. A backgrounded process is
  session-scoped (it stops when Mermaid exits).

### Changed

- **System prompt steers agents off blocking commands.** For smoke checks,
  prefer a finite command (a build, a one-shot test run, `--version`) over a
  dev server or watcher (which never exit and block until the 30s foreground
  timeout); use `mode="background"` for servers/daemons/watchers.

## [0.10.1] - 2026-06-22

### Fixed

- **"Running tools…" looked frozen.** The tool-execution status line was stuck
  at `0s` and didn't say what was running, so a slow tool call (e.g. an agent
  running `npm run dev` as a smoke test — a dev server that blocks until
  `execute_command`'s 30s timeout) looked hung. `TurnState::ExecutingTools` was
  the only active turn state without a `started` timestamp, so the elapsed clock
  was hard-coded to 0 (Generating/Compacting tick normally). It now carries a
  start time and counts up, and the status line **names the in-flight tool**
  (e.g. `Running tools: Bash npm run dev`, with `+N more` when several run in
  parallel). Not a hang — `Esc` always aborted it.

## [0.10.0] - 2026-06-22

Headline: **durable, agent-managed memory** — Mermaid now remembers facts across
sessions in plain Markdown files it reads and maintains itself. Alongside it, a
batch of TUI upgrades (richer markdown, drag-select + copy, a chat scrollbar,
inline approval prompts with an arrow-key picker, the version in the footer) and
a round of paste/safety/diff polish.

### Added

- **Durable agent memory.** Mermaid keeps long-term semantic memory as plain
  Markdown files — one atomic fact per file — across three scopes: global,
  project-private (the default; machine-local, not committed), and
  project-shared (opt-in, committed to `.mermaid/memory/`). An auto-derived
  index is always in context; the agent reads a fact on demand via `read_file`
  and maintains memory itself through a `memory` tool (`remember` / `update` /
  `forget`), ungated in every safety mode except read-only. Manual controls:
  `/memory` lists, `/remember <fact>` saves, `/forget <name>` deletes, and
  `/consolidate-memory` runs a model-assisted, checkpoint-reversible **prune**
  of duplicate/stale facts (prune-only by design — stored facts are never
  rewritten, which avoids semantic drift). Secrets, tokens, and PII are never
  stored. The index is generated from the files, so it can't drift from them;
  no database, vectors, or embeddings.
- **Version in the status footer.** The footer's second line now reads
  `mermaid vX.Y.Z · safety: <mode> · reasoning: <level>` — the version tracks
  the crate version automatically.
- **Chat scrollbar.** The transcript now shows a scrollbar (ratatui 0.30's
  `Scrollbar`) in a reserved right-hand gutter whenever it overflows the
  viewport, so you can see scroll position at a glance. Dropped the unused
  `palette` and `macros` ratatui features for a leaner build; kept
  `scrolling-regions`.
- **Richer markdown rendering.** Assistant markdown is now theme-aware
  (headings, lists, blockquotes, links, tables all use the active theme's
  palette instead of hardcoded ANSI colors), fenced code blocks get **in-house
  syntax highlighting** (keywords / strings / line-comments via a small
  language-agnostic lexer, no new dependency) on the theme's code background,
  code-block **indentation is preserved** (code lines are no longer word-wrapped
  into a collapsed paragraph; lines wider than the viewport soft-wrap with a
  hanging indent rather than being clipped), inline `code` is tightened (no
  stray padding), link destinations are shown dimmed after the text, and `---`
  thematic breaks render as a horizontal rule.
- **Drag to select, Ctrl+Shift+C to copy.** A plain left-mouse drag selects
  chat text (reverse-video highlight); **`Ctrl+Shift+C` copies** the selection
  to the system clipboard (with a "Copied N chars" status). Selecting and
  copying are distinct — a drag never auto-copies, so it can't clobber your
  clipboard. Mouse-wheel scroll and Ctrl+Click-to-open-image are unaffected.
  Selection is display-cell accurate (CJK-safe), drops the rendered left
  margin so multi-line and code copies are clean (the code's own indentation
  is kept), and clears on scroll. `Shift+Drag` still bypasses to native
  terminal selection. Copy shells out to the platform tool (`clip`/PowerShell,
  `pbcopy`, `wl-copy`/`xclip`) — no new dependency.
- **Inline approval prompts.** In interactive `ask` mode (and `auto`-mode
  escalations), a gated tool action now **pauses and prompts inline** —
  `1` Yes · `2` Yes, don't ask again (per-tool; `execute_command` keyed on the
  program) · `3`/Esc No — and the agent waits for the answer. Previously `ask`
  mode just returned an "Approval required" error and the model flailed; the
  only approval path was the out-of-band `/approve <id>` flow (still used in
  headless `mermaid run`). The prompt also covers the previously-unguarded
  non-replayable tools (web / MCP / subagent / computer-use) under `ask`. The
  picker is keyboard-navigable — `↑`/`↓` move a highlighted option and `Enter`
  selects it, or press the number directly.

### Changed

- **BREAKING: memory is greenfield-replaced.** The old SQLite/JSONL key-value
  memory store is gone, along with its CLI subcommands; `/memory`, `/remember`,
  and `/forget` are now backed by the new Markdown file store (and `/memory-edit`
  is removed). There is no migration — saved entries from the old store are not
  carried over.
- **System prompt — interaction & editing norms.** Added a focused set of
  cross-model norms: no time estimates; make the smallest change that does the
  task (no speculative abstractions/options, no cleanup of untouched code, no
  backwards-compat shims or tombstone comments); don't create files (esp.
  docs/README) unless needed; don't introduce security holes; communicate in
  response text, not via tool calls/comments; treat file/web/tool output as
  untrusted data, not instructions; and stronger anti-sycophancy (skip
  "You're absolutely right"-style validation, investigate over confirming).
- **Project instruction files are now exactly `AGENTS.md` + `MERMAID.md`.**
  `CLAUDE.md` and `GEMINI.md` are no longer auto-loaded. `AGENTS.md` (the
  cross-tool open standard) loads first; `MERMAID.md` (mermaid-specific) loads
  last and overrides it on conflict. **BREAKING** for anyone relying on
  CLAUDE.md/GEMINI.md auto-loading. (The `find_mermaid_md` back-compat helper
  was removed.)

### Fixed

- The `/clear` (and other) confirmation modal was inert — nothing rendered it
  and no key handler read it. It now shows and accepts `y`/`n`.
- **Chat spacing:** a turn that thought and then immediately called a tool
  (hidden reasoning + empty text + actions) rendered the "Reasoning hidden"
  placeholder flush against the first tool block. It now gets the same single
  blank-line gap every other block has.
- **Multi-line paste (Windows).** Pasting multi-line text submitted each line
  as its own message and rendered character-by-character. crossterm 0.29
  doesn't emit `Event::Paste` on the Windows console — a paste arrives as a
  burst of individual key events, so every newline hit the Enter→submit path.
  The main loop now coalesces a rapid key burst (characters, newlines, and
  tabs) into a single atomic paste (a lone Enter still submits, a lone Tab is
  still a Tab; Shift+Enter still inserts a newline), and the input box renders
  embedded newlines as real rows.
- **System prompt** refreshed: it now documents the safety/permission modes
  (and how to behave when an action is gated — explain, don't spam retries),
  the tool set, and the in-session controls.
- **Pasted text scrambled on Windows.** A paste whose burst split into coalesced
  `Paste` chunks plus stray `Char` key events (uppercase letters) came out
  reordered — e.g. `Review … Define … Report …` became `RDReview … efine …
  eport …`. Paste now inserts at the cursor and advances it, exactly like
  typing, so the result stays in order however the burst splits.
- **The model couldn't see the live safety mode.** After switching modes
  mid-session (e.g. `read_only` → `full_access`), the agent kept refusing
  actions based on a stale gate error. The current mode is now surfaced in the
  prompt each turn (the same field the policy gate enforces), so it stops
  guessing from old errors.
- **Slash palette hid hyphenated commands.** Typing a command's first word
  (e.g. `/consolidate`) matched nothing until the hyphen was typed; plain
  prefix matching now surfaces `consolidate-memory`, `cloud-setup`, etc.
- **Duplicate `/compact` indicator.** Removed the redundant gray "Compacting
  context…" status line; the live blue indicator and the completion receipt
  remain.
- **Diff header clutter.** File diffs no longer print the `---`/`+++`/`@@`
  unified-diff headers above the change — just the success line and the colored
  body.

### Security

- Bumped `quinn-proto` 0.11.14 → 0.11.15 for RUSTSEC-2026-0185 (a remote
  memory-exhaustion DoS in out-of-order QUIC stream reassembly).

## [0.9.0] - 2026-06-21

Headline: a **classifier-backed Auto safety mode** (as in Claude Code / Codex)
plus in-session mode switching. The minor bump reflects the breaking rename of
the `AutoReview` mode.

### Added

- **Auto safety mode** — a classifier-backed permission mode (as in Claude Code
  and Codex). Under `[safety] mode = "auto"`, borderline actions (shell /
  network / external tools) are vetted by an LLM against your stated intent:
  aligned actions run automatically, risky or off-task ones escalate to an
  approval prompt. Reads and file edits still auto-run (with checkpoints), and
  destructive patterns stay hard-denied by the rule engine. The classifier
  defaults to the session model, overridable via `[safety] auto_classifier_model`;
  any classifier error or timeout fails safe (escalate), never silently allows.
- **In-session safety switching** — `Shift+Tab` cycles `read_only → ask → auto →
  full_access`, and `/safety [mode]` (alias `/permission`) shows or sets it.
  Both are session-scoped (the `[safety] mode` config value remains the
  persistent default), and the status footer now always shows the active mode.

### Changed

- **BREAKING:** the `AutoReview` safety mode is renamed to `Auto`, and its
  behavior changed from rule-based ("ask for everything risky") to
  classifier-backed. Config files with `[safety] mode = "auto_review"` must
  change the value to `"auto"` — the old string is no longer accepted.
- Bumped dependencies: `rusqlite` 0.39 → 0.40, `sha2` 0.10 → 0.11, `getrandom`
  0.3 → 0.4.

### Fixed

- Loosened a flaky timing assertion in the cancellation/timeout integration
  test (`execute_command_timeout_honored`) that intermittently failed on loaded
  CI runners; it now measures the real timeout behavior with a generous ceiling.

### CI

- Bumped pinned GitHub Actions: `actions/checkout` → v7, `actions/download-artifact`
  → v8, `actions/upload-artifact` → v7, `softprops/action-gh-release` → v3.

## [0.8.1] - 2026-06-21

Documentation, release-pipeline, and supply-chain fixes on top of 0.8.0. This
is the first 0.8.x release published to crates.io (the workspace split had
broken crates.io publishing after 0.7.1).

### Changed

- **README** corrected against the code: repointed the install instructions
  (GitHub Release binaries / `cargo install --git`, since crates.io can lag),
  fixed the Alt+T reasoning cycle (all seven levels) and the daemon defaults
  (TCP off by default via `MERMAID_DAEMON_ENABLE_TCP`, socket `0600` / data dir
  `0700`), documented `mermaid pr create`, the `MERMAID_ALLOW_PLUGIN_FETCH`
  plugin opt-in, and the `[safety]` config section.

### Fixed

- **Restored crates.io publishing.** `mermaid-runtime` is now publishable and
  the release workflow publishes it before the `mermaid-cli` binary crate
  (the path dependency now carries a version requirement).

### Security

- Pinned every GitHub Action to a commit SHA and added a Dependabot config to
  keep them current; the release workflow now attaches a `SHA256SUMS` file to
  each GitHub Release so downloaded binaries can be verified.

## [0.8.0] - 2026-06-21

Security-hardening release: the full-codebase review's critical/high findings
are fixed, dependency CVEs are patched, and the safety defaults are now
safe-by-default. Also adds Git-host PR creation.

### Added

- **`mermaid pr create`.** Create a pull/merge request from the current
  branch via the host's own CLI (`gh` for GitHub, `glab` for GitLab),
  reusing its existing authentication. Auto-detects the host from the
  `origin` remote (overridable with `--provider`), and supports `--title`,
  `--body`, `--summary <file>` (attach a review summary), `--base`,
  `--draft`, and `--web`. (#2)

### Changed

- **BREAKING: default safety mode is now `Ask` (was `FullAccess`).** A fresh
  install prompts for approval on mutations / shell / network actions instead
  of auto-running them. Set `[safety] mode = "full_access"` in config to
  restore the old behavior.
- **BREAKING: the daemon TCP control listener is off by default.** Opt in with
  `MERMAID_DAEMON_ENABLE_TCP=1` (the old `MERMAID_DAEMON_DISABLE_TCP` toggle is
  gone), and auth is now required for every TCP command including `health`.
- **BREAKING: installing a plugin from a Git URL now requires
  `MERMAID_ALLOW_PLUGIN_FETCH=1`** and no longer auto-expands a bare
  `owner/repo` into a GitHub URL; the clone runs with repo hooks and external
  transports disabled.
- Shell-command risk classification was rewritten to tokenize the command:
  unknown commands now require approval instead of being treated as read-only,
  and network/interpreter commands (`curl`, `wget`, `ssh`, `python -c`, …) are
  classified as network/process actions.

### Security

- The safety policy is now enforced for **every** dangerous tool. Previously
  `web_*`, `mcp`, `subagent`, and the computer-use tools bypassed it entirely,
  so `ReadOnly` silently failed to block them; a single gate now covers them.
- Provider API keys and the daemon token are scrubbed from the environment of
  commands spawned by `execute_command`, MCP servers, and plugin hooks.
- Filesystem path containment now resolves through the canonical
  nearest-existing ancestor (closes symlink-follow / TOCTOU and
  symlinked-parent-on-create escapes) and fails closed.
- Daemon control socket is created `0600` and the data dir `0700`; the
  conversation `/load` id is validated against the generated format.
- Bounded the previously-unbounded command output capture and the streamed
  tool-call index (anti-OOM/DoS).
- Session, compaction-archive, and checkpoint writes are now atomic
  (temp + fsync + rename); SQLite opens with WAL + `busy_timeout`.
- Patched **12 RUSTSEC advisories** via dependency updates: `aws-lc-sys`
  0.37 → 0.41, `rustls-webpki` → 0.103.13, plus `bytes`, `quinn-proto`, `time`.

### Fixed

- Streaming `Done` no longer races ahead of buffered tool calls (the
  intermittent "model forgot to call the tool" bug).
- Token estimates now count assistant tool-call argument bytes, fixing
  systematic under-compaction that could overflow the provider context.
- Anthropic: drop assistant `thinking` blocks that lack a signature (they
  caused a 400 on the next turn). Gemini: a safety/recitation-blocked response
  is surfaced as a structured error instead of a misleading parse failure.
- Compaction now persists the archive before overwriting the (message-stripped)
  conversation, so a failed archive write can no longer lose messages.

## [0.7.1] - 2026-04-26

Runtime hardening, typed tool output, token accounting, and context
compaction on top of the v0.7 architecture.

### Added

- **Manual and automatic context compaction.** `/compact [instructions]`
  now creates a model-visible checkpoint, archives the removed raw
  messages under `.mermaid/compactions/`, and replaces old history
  with a structured handoff plus the most recent turns. Mermaid also
  auto-compacts near the model's context limit and retries once after
  provider context-limit errors.
- **Typed tool-result metadata.** Tool outcomes now carry structured
  status, duration, line counts, byte counts, result counts, artifacts,
  and tool-specific metadata. The TUI renders friendly summaries such
  as read/write line counts, web-search result counts, command exit
  status, and background process details without scraping model-facing
  text.
- **Runtime metadata layer.** `domain::runtime` tracks lifecycle
  signals, provider capability snapshots, managed background
  processes, tool metadata, and a lightweight runtime timeline.
- **Background command registration.** `execute_command` can run
  long-lived commands in background mode, capture startup logs, detect
  local URLs, and register PID/log metadata for Mermaid to display and
  persist.
- **Subagent tool.** The `agent` tool can spawn autonomous child agents
  using the active model/provider, with depth limits and a child tool
  registry that excludes unsafe/self-recursive tools.
- **Computer-use tools.** The v0.7 tool registry now includes the
  screenshot, click, mouse-move, keypress, type-text, scroll, and
  window-list computer-use tools.
- **Chat image artifacts.** Tool-produced images can be attached to
  assistant messages, rendered in the chat history, and opened from the
  TUI.
- **Lifecycle signal handling.** SIGINT, SIGTERM, and SIGHUP now flow
  through reducer messages so Mermaid can restore the terminal and save
  state consistently.
- **Context and usage slash commands.** `/usage` and `/context` report
  provider token usage, session totals, estimated prompt budget, model
  context capacity, and recent compaction metadata.

### Changed

- **v0.6 runtime deleted.** The `MERMAID_V7=1` opt-in is gone; the
  v0.7 architecture is now the only code path. `src/tui/`,
  `src/runtime/`, `src/agents/`, `src/models/backend.rs`, and
  `src/models/retry.rs` are all removed. Net ~8,000 LOC of old code
  gone from the tree.
- Non-interactive `mermaid run <prompt>` now runs on the v0.7 reducer
  + effect runner (same as interactive); output shape matches the
  v0.6 `NonInteractiveResult` so scripts keep working.
- Slash commands, diff helpers, action value types, MCP manager
  accessor, and the web search client all moved out of the v0.6
  namespace into `src/domain/`, `src/render/`, `src/mcp/`, and
  `src/providers/tool/`. No behaviour changes — just no longer
  reaching back into deleted modules.
- Token accounting now distinguishes provider-reported usage from
  local estimates. The footer shows current context usage separately
  from last API usage and cumulative session totals, avoiding the old
  inflated "session tokens" display.
- Model/provider requests now use a stream bridge shared across
  providers, making cancellation and done/usage events more uniform.
- Terminal teardown now restores raw mode, mouse capture, bracketed
  paste, and the alternate screen before asynchronous shutdown work
  drains.

### Fixed

- Ctrl+C from an idle, empty TUI exits and restores the user's terminal
  reliably instead of requiring repeated keypresses or leaking terminal
  escape sequences back into the shell.
- Cancelled turns now drain through `TurnCancelled`, preventing stale
  provider/tool events from leaving the reducer stuck in `Cancelling`.
- Tool cancellation now returns typed cancelled outcomes instead of
  relying on textual placeholders.
- Stale screenshots are evicted from outgoing model requests while the
  latest relevant image remains available in chat history.
- Gemini API key resolution now documents and preserves the
  `GEMINI_API_KEY` legacy fallback alongside `GOOGLE_API_KEY`.

### Removed

- Two integration test files that exercised the v0.6 runtime
  (`tests/agent_loop_tests.rs`, `tests/tui_behavior_tests.rs`). The
  reducer + effect parity suites (`tests/reducer_flows.rs`,
  `tests/effect_cancel.rs`) cover the equivalents.

### Added (free, via the new architecture)

- MCP servers initialize automatically at startup via
  `Cmd::InitMcpServers`. v0.6 only init'd in the interactive path;
  non-interactive invocations now get MCP tools too.
- `manager_ref::wait_ready()` — if a tool call races startup, it
  parks briefly for init to complete instead of immediately
  erroring.
- `--record <file>` now records structured reducer input events,
  including lifecycle and compaction events, for replay/debugging.

### Docs

- Updated the architecture, adding-tools, replay-debugging, and README
  docs for the v0.7-only runtime, typed metadata, background commands,
  computer-use path changes, and current provider key behavior.

### Tests

- Added regression coverage for terminal-mode restoration on Ctrl+C,
  context compaction planning/replacement, slash-command parsing,
  compact event rendering, token-status rendering, background command
  metadata, and subagent/tool registry behavior.

## [0.7.0] - 2026-04-21

The Architecture Release. This is a big-bang rewrite of Mermaid's
runtime on the Elm/MVU pattern: one pure reducer, effects as data,
structured concurrency per turn. External behaviour is intended to
match v0.6; several whole classes of bug that v0.6 let slip become
impossible to express against the new types.

The new path ships behind `MERMAID_V7=1` for the v0.7.0 release so
the v0.6 runtime keeps running by default during the migration
window. Flipping the default happens in a follow-up once the v7
path has been exercised against real sessions.

### Added

- **Pure reducer** (`src/domain/reducer.rs`) — `fn update(State, Msg)
  -> (State, Vec<Cmd>)`. Synchronous. Stale events filter by embedded
  `TurnId` before any state transition. Tool-result completeness is
  type-enforced (`Vec<Option<ToolOutcome>>` can't advance to the
  follow-up call until every slot is `Some(_)`). Exhaustive match on
  `Msg`; clippy catches any missing variant.
- **Effect runner** (`src/effect/`) — the single place in the
  codebase where tokio tasks spawn. Owns per-turn `TurnScope`
  (`CancellationToken` + `JoinSet`) so cancellation is a signal, not
  a poll. Retry/tracing middleware (from v0.6's
  `src/models/retry.rs`) now wraps any adapter uniformly.
- **`ModelProvider` + `ToolExecutor` traits** (`src/providers/`) —
  the adapter surface. Four model providers (Ollama, Anthropic,
  Gemini, OpenAI-compat) + six built-in tools (read_file,
  write_file, edit_file, delete_file, create_directory,
  execute_command) all implement these. MCP dispatch lives at
  `tool::McpToolProxy`.
- **`StreamContext` + `ExecContext`** — typed per-call contexts
  carrying the turn's cancellation token. Providers and tools that
  ignore the token don't get past code review; the type signature
  makes the race explicit.
- **Pure view function** (`src/render/`) — `fn render(&State, &mut
  Frame)`. Never mutates state, never performs I/O, never holds a
  `&mut App`. Testable against ratatui's `TestBackend` without a
  runtime or terminal.
- **Single event loop** (`src/app/run.rs`) — one `tokio::select!`
  over crossterm `EventStream` + effect-result mpsc + tick timer.
  Replaces v0.6's two competing event loops. Behind
  `MERMAID_V7=1`.
- **`TerminalGuard`** — raw-mode/alt-screen setup with panic-safe
  teardown. A panic mid-render restores the shell.
- **Recorder / Replay** (`src/app/recorder.rs`) — JSONL msg logs.
  Reducer is event-sourced by design, so record is one line per
  reducer input and replay is a fold. Regression tests as flat
  files; bug reports as replay logs.

### Changed

- `ExecuteCommandTool` now races subprocess wait against the turn's
  cancellation token. Ctrl+C during a long-running build aborts
  within microseconds (plus SIGKILL travel) instead of waiting for
  the 300-second timeout. Structural fix for the v0.6 "20-press
  Ctrl+C" report — tokens can't be forgotten; they're in the type.
- Retry middleware moves from `src/models/retry.rs` (deleted in
  follow-up) to `src/effect/middleware.rs`. Behaviour identical:
  3 attempts, 500ms→3s exponential backoff, retry on 5xx / 429 /
  `ConnectionFailed`.

### Docs

- `docs/architecture.md` — full tour of the new design + invariants.
- `docs/adding_tools.md` — one-file tool recipe.
- `docs/adding_providers.md` — adapter recipe.
- `docs/replay_debugging.md` — record/replay usage.

### Tests

- 558 tests pass: 516 library, 42 integration.
- `tests/reducer_flows.rs` — 15 multi-message flow tests (stale
  events, tool-outcome completeness, cancel, quit, slash commands).
- `tests/effect_cancel.rs` — 5 real-tokio tests (Ctrl+C aborts a
  `sleep 60` within 300ms; bounded shutdown).
- Ratatui `TestBackend` renderer tests (5).

### Not yet in v0.7.0

- Default binary path still runs v0.6 runtime. Flip happens in a
  follow-up release once v7 parity is verified against real
  sessions.
- Subagent dispatch, MCP startup, and several modals (conversation
  load, /cloud-setup, model list) still route through v0.6 code.
  Reducer has the `Msg` vocabulary; implementations mechanical.

## [0.6.0] - 2026-04-16

Major release: multi-provider adapter support. Mermaid is no longer
Ollama-only — direct integrations for Anthropic Claude, Google Gemini, and
the full OpenAI-compatible long tail (OpenAI, Groq, OpenRouter, Cerebras,
DeepInfra, Together). Plus a new slash-command palette, auto-loaded
MERMAID.md project instructions, MCP spec bump, and a security update.

### Added

- **Anthropic adapter** (`src/models/adapters/anthropic.rs`) — bespoke
  Messages API support: `2023-06-01` version pin, adaptive + legacy
  thinking formats dispatched per model, typed SSE streaming, `thinking`
  signature round-trip for multi-turn extended thinking, `cache_control:
  ephemeral` on system prompts + last tool for prompt caching, vision
  (base64 images), tool translation to Anthropic's flat `{type: "custom"}`
  shape. Supports Claude Opus 4.7 (`xhigh` effort tier), Sonnet 4.6,
  Opus 4.6, Sonnet 4.5, Opus 4.5, Haiku 4.5.
- **Gemini adapter** (`src/models/adapters/gemini.rs`) — per-method
  endpoints (`:generateContent` / `:streamGenerateContent?alt=sse`),
  `user`/`model` role convention, `functionResponse` merge for tool
  results, per-model thinking dispatch (Gemini 3 `thinkingLevel` enum,
  Gemini 2.5 Pro/Flash/Flash-Lite `thinkingBudget` with correct floors,
  2.0 omits `thinkingConfig`), `thought: true` reasoning parts, inline
  base64 images for vision. Curated list: `gemini-pro-latest`,
  `gemini-flash-latest`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`,
  `gemini-3.1-flash-lite-preview`, `gemini-2.5-pro/flash/flash-lite`.
- **OpenAI-compatible adapter** (`src/models/adapters/openai_compat.rs`)
  — single `/chat/completions` adapter with per-provider quirks encoded
  in `ProviderProfile`. Built-in registry: OpenAI, Groq, OpenRouter,
  Cerebras, DeepInfra, Together. Three reasoning strategies (`Effort`,
  `OpenRouterShape`, `None`) and three extraction strategies
  (`DeltaContentField`, `InlineThinkTags`, `None`). Streaming tool-call
  accumulator handles OpenAI's chunked `delta.tool_calls` pattern.
  OpenRouter `X-OpenRouter-Title` canonical header.
- **Custom OpenAI-compatible providers** — users can add any
  `/chat/completions` endpoint via `[providers.<name>]` in `config.toml`
  with `base_url`, `api_key_env`, and `compat = "openai" |
  "openai-effort" | "openrouter"`.
- **`ReasoningLevel` enum** (`src/models/reasoning.rs`) — seven tiers
  (`None`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`) with rank
  ordering; `XHigh` sits between `High` and `Max`. `nearest_effort()`
  snaps user choice onto the model's advertised `ReasoningCapability`.
  Per-model persistence via `[reasoning_per_model]` in config.
- **`--reasoning <level>` CLI flag** overrides config-default for this
  session.
- **Typed streaming** (`src/models/stream.rs`) — `StreamEvent` enum
  (`Text`, `Reasoning`, `ToolCall`, `Done`) replaces the legacy text-only
  callback. Adapters emit typed events; consumers route them without
  marker-sniffing.
- **`ModelCapabilities`** (`src/models/capabilities.rs`) — per-model
  `supports_tools`/`supports_vision`/`supports_reasoning`/`max_context_tokens`
  advertised by each adapter.
- **MERMAID.md project instructions** (`src/app/instructions.rs`) —
  walks UP from cwd to the git root or `$HOME`, loads the nearest
  `MERMAID.md`, auto-reloads on mtime change before every model call
  (one stat per turn). 10k-token cap with truncation marker. Injected
  via `ModelConfig::dynamic_system_suffix`; Anthropic gets a separate
  cache block (static base stays warm across project switches).
- **Slash-command palette** (`src/tui/widgets/slash_palette.rs`,
  `src/tui/slash_commands.rs`) — type `/` to open a filter-as-you-type
  list of all commands. Up/Down navigates, Tab completes, Enter
  dispatches, Esc dismisses. Centralized `COMMAND_REGISTRY` so `/help`
  auto-updates with new commands.
- **`/reasoning <level>` slash command** — per-model persisted reasoning
  depth. Alt+T cycles `None → Low → Medium → High → Max → None`
  (Minimal + XHigh reachable only via `/reasoning`, treated as
  specialist tiers).
- **MCP 2025-11-25 protocol** — bumped from 2025-03-26. New content
  block types: `audio`, `resource_link`, `resource` (embedded).
  Audio flows through the image attachment channel; resource links
  render as text so the model can follow up with another tool call.
- **`postgres-mcp` (uvx)** replacing deprecated
  `@modelcontextprotocol/server-postgres` — crystaldba community
  maintainer. Env var renamed `DATABASE_URL` → `DATABASE_URI`.
- **`@zencoderai/slack-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-slack` — Zencoder is the official
  handoff maintainer.
- **`@brave/brave-search-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-brave-search` — Brave is now the
  first-party maintainer.
- **Graceful MCP shutdown** (`src/mcp/transport.rs`) — close stdin →
  2s wait → SIGTERM → 1s wait → SIGKILL. Replaces the previous
  straight-to-SIGKILL path.
- **UTF-8-safe byte-buffer draining** (`src/utils/ndjson.rs`,
  `src/utils/sse.rs`) — NDJSON line splitter and SSE event splitter
  that buffer raw bytes and decode only complete frames. Protects
  against TCP-chunk-inside-codepoint corruption on both Ollama NDJSON
  and OpenAI-compat SSE streams.
- **API-key resolution** (`src/utils/auth.rs`) — uniform env-var lookup
  with optional `[providers.<name>].api_key_env` override.

### Changed

- **`/` slash-command prefix** replaces the legacy `:` colon prefix.
  All commands now live under `/`; the palette only opens for `/`.
- **Tokio bumped to 1.44** (resolves to 1.49 via caret) — closes
  RUSTSEC-2025-0023 (broadcast channel unsoundness).
- **toml 0.9 → 1.1** (major version bump), clap 4.5 → 4.6, bytes 1.8 →
  1.11, regex 1.11 → 1.12.
- **Removed `dotenvy`** — unused dead dependency.
- **Added `temp-env`** dev-dep — replaces `unsafe { env::set_var /
  remove_var }` in `backend.rs` + `auth.rs` tests; safer under
  `--test-threads > 1`.
- **MSRV pinned to 1.91** in `Cargo.toml rust-version`, `.clippy.toml`,
  and `flake.nix`. `str::floor_char_boundary` (used for UTF-8-safe
  slicing) stabilized in Rust 1.91.
- **Ollama `gpt-oss` dispatch** — sends `think: "low"|"medium"|"high"`
  (string enum) instead of the bool other Ollama models expect. Advertised
  as `Levels([None, Low, Medium, High])` in capabilities so XHigh/Max
  snap correctly via `nearest_effort`.
- **OpenRouter header** — `X-Title` → `X-OpenRouter-Title` (the new
  canonical name; old still accepted for backward compat).

### Fixed

- Anthropic `Max` effort now gated per-model — Sonnet 4.5 / Opus 4.5 /
  Haiku 4.5 snap to `"high"` since they don't accept `"max"` per the
  2026-04 effort documentation.
- Gemini 3 model IDs refreshed — `gemini-3-pro` (shut down 2026-03),
  `gemini-3-flash`, `gemini-3-flash-lite` replaced with the current
  `-preview` variants and `-latest` aliases.
- MCP registry entries for `slack`, `postgres`, and `brave-search` swapped
  to current maintained alternatives (originals deprecated upstream).

### Removed

- `src/tui/stream_handler.rs` — replaced by the typed `StreamEvent` path
  and `TuiObserver` in `loop_coordinator.rs`.
- `.env.example` — `dotenvy` dependency removed, no `.env` loading path.

## [0.5.1] - 2026-04-12

### Added
- MCP (Model Context Protocol) client integration for connecting to external tool servers
- `mermaid add <name>` / `mermaid remove <name>` / `mermaid mcp` commands for MCP server management
- Built-in registry of 17 popular MCP servers (context7, github, playwright, memory, postgres, etc.)
- Enhanced computer use: window-aware screenshots (`mode: "window"`), `list_windows` tool, auto-screenshot after click/type/key actions
- 42 new tests across agent loop, session persistence, non-interactive mode, and stream handler

### Changed
- MCP servers now initialize in background (TUI renders immediately instead of blocking startup)
- MCP tools become available to the model as soon as servers are ready, even mid-agent-loop
- Centralized model configuration into `ModelConfig::from_app_config()` (internal refactor)
- Nix flake: switched from nightly Rust to stable 1.87.0, removed OpenSSL 1.1 dependency

### Fixed
- Ollama URL normalization with paths (`http://host/v1` no longer appends port after the path)
- Token tracking now counts total tokens (prompt + completion) instead of completion-only
- `ActionResult` images field properly propagated for screenshot tool results
- Command timeout treated as success (process continues running in background)

### Removed
- Sync `list_models()` from Ollama detector (replaced by async-only `list_models_async()`)
- Unused fields from MCP `ResolvedServer` struct

## [0.5.0] - 2026-03-15

### Added
- Ollama Cloud setup flow (`:cloud-setup` command and interactive API key configuration)
- Claude Code-style subagents for parallel task execution (`agent` tool)
- TUI stream event system (typed `StreamEvent` enum replacing string-based protocol)
- Image paste status widgets and attachment management UI
- Web search and web fetch via Ollama Cloud API (`web_search`, `web_fetch` tools)

### Changed
- Consolidated ModelFactory into `backend.rs`, removed `factory.rs`
- Architectural cleanup across TUI state management (split App into focused state modules)
- Consolidated git tools into bash `execute_command`, removed `git2` dependency
- Removed vestigial LiteLLM infrastructure

## [0.4.1] - 2026-02-10

### Fixed
- Full codebase review: 53 clippy warnings fixed
- Security and correctness improvements across all modules

## [0.4.0] - 2026-02-08

### Changed
- Codebase review, dead code removal, and architectural cleanup

### Fixed
- Gate crates.io publish behind `PUBLISH_TO_CRATES_IO` environment variable

## [0.3.0] - 2026-01-20

### Added
- Proper agent loop for native Ollama tool calling (replaces text-based action blocks)
- Thinking mode toggle with Alt+T (for models that support extended reasoning)
- Message queuing — type while model generates, messages send in order
- Session persistence — start fresh by default, `--continue` to resume last conversation
- `--sessions` flag to pick a previous conversation to resume
- Model persistence — last-used model saved to config
- Image paste support with vision model integration (Ctrl+V)
- `edit_file` tool for targeted text replacement with diff display
- Web search via Ollama Cloud API (replaced SearXNG)
- `web_fetch` tool for fetching URL content as markdown
- Bracketed paste support for multi-line input
- Markdown table rendering in chat
- Auto-pull models from Ollama when not found locally
- Non-interactive mode: `mermaid run "prompt"` with JSON/text/markdown output
- `delete_file` and `create_directory` tools
- Computer use tools: `screenshot`, `click`, `type_text`, `press_key`, `scroll`, `mouse_move`
- File-based logging (writes to `~/.mermaid/mermaid.log` instead of corrupting TUI)
- Esc/Ctrl+C to interrupt queued message processing

### Changed
- Simplified to Ollama-only backend (removed vLLM, proxy, model router)
- Overhauled system prompt for tool-calling models
- Upgraded to Rust 2024 edition
- Upgraded ratatui from 0.29 to 0.30 with new idioms
- Rewrote README for accuracy and simplicity
- Unified tool calling to native Ollama API format
- Used Ollama's real token counts (removed tiktoken dependency)

### Fixed
- Char-boundary safe string slicing throughout (prevents UTF-8 panics)
- False positive in dangerous command detection (`.mermaid` matching `rm`)
- Cursor position drifts on wrapped input lines
- Cursor jumps to column 1 when typing space
- Timestamp overlapping long user messages
- Streaming timeout for large operations (removed global HTTP timeout)
- Windows duplicate input from key release events

## [0.2.1] - 2025-12-18

### Added
- NixOS support via `flake.nix` and `flake.lock`
- `delete_file` and `create_directory` tools for agent
- `.vs/` to `.gitignore`

### Changed
- Organized project root directory structure:
  - Moved scripts to `scripts/`
  - Moved infrastructure files to `infra/`
- Updated README to accurately reflect available tools (renamed `run_command` to `execute_command`)

### Fixed
- CLI argument conflict: `prompt` now uses `-P` short flag, `path` retains `-p`
- `resume` argument `conflicts_with` validation logic
- Compilation errors due to relative paths in `include_str!` macros

## [0.2.0] - 2025-11-16

### Added
- Native Ollama tool calling support with JSON Schema tool definitions
  - 9 tools: read_file, write_file, run_command, git_status, git_diff, git_commit, web_search, list_directory, get_file_info
  - Structured tool definitions with detailed parameter descriptions
  - Tool calls parsed from streaming chunks in real-time
- Enhanced input widget UI matching Claude Code aesthetics
  - Always-visible "> " prompt prefix
  - Full-width input bar with top/bottom borders only
  - Proper text wrapping with 2-space indentation on continuation lines
  - Blank line after "Thinking..." marker for better visual spacing
- Model compatibility framework for tool calling detection

### Changed
- Migrated from text-based action blocks to Ollama native tool calling API
- Completely rewrote system prompt (76% reduction: 353 to 86 lines)
- Tool definitions now provide comprehensive usage guidance
- Cleaner, more maintainable architecture with dedicated tools module
- Updated all backend adapters to support tool_calls in responses
- Stream handler now accumulates tool calls from streaming chunks

### Removed
- Legacy text-based parsers (parser.rs, extractor.rs, segmenter.rs)
- Verbose system prompt with action block examples
- Text-based action block parsing (temporarily, will be restored as fallback)

### Fixed
- Cursor positioning now accounts for "> " prefix and border changes
- Text wrapping alignment issues with continuation lines
- Input widget rendering for empty input states

### Breaking Changes
- Models without native Ollama tool calling support will not execute actions
- Next release (v0.2.1) will restore text-based fallback for universal compatibility
- Compatible models: llama3.1, llama3.2, qwen2.5-coder, mistral-nemo, firefunction-v2

## [0.1.1] - 2025-09-27

### Added
- Test helper functions for better test coverage
  - `path_exists` function in filesystem module for path validation
  - `current_branch` function in git module for branch detection

### Fixed
- Test compilation errors in filesystem and git modules
- Clippy configuration to allow reasonable nesting depth

### Changed
- Adjusted CI/CD workflow clippy strictness to warnings level

## [0.1.0] - 2025-09-27

### Added
- Initial release of Mermaid CLI
- Model-agnostic AI pair programmer with support for 100+ LLM providers via LiteLLM proxy
- Terminal User Interface (TUI) built with Ratatui
  - Real-time streaming responses
  - Syntax highlighting for code
  - Project sidebar with file tree
  - Markdown rendering support
- Agentic capabilities
  - File operations (read, write, create, delete)
  - Git integration (diff, status, commit)
  - Shell command execution
  - Project context awareness
- Configuration system
  - Global config at ~/.config/mermaid/config.toml
  - Project-specific config support
  - Environment variable configuration
- LiteLLM proxy integration
  - Support for OpenAI, Anthropic, Google, Ollama, and 90+ more providers
  - Unified API interface
  - Docker/Podman containerization
- Project context loading
  - Automatic project structure analysis
  - Token counting and management
  - Respects .gitignore patterns
- GitHub Actions CI/CD workflows
  - Automated testing and linting
  - Multi-platform release builds (Linux, macOS, Windows)
  - Security vulnerability scanning
  - Code formatting enforcement
- Dual licensing (MIT OR Apache-2.0)

### Infrastructure
- Rust 2021 edition
- Comprehensive test suite
- rustfmt and clippy configuration
- Docker compose setup for LiteLLM proxy

[Unreleased]: https://github.com/noahsabaj/mermaid-cli/compare/v0.19.1...HEAD
[0.19.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.19.0...v0.19.1
[0.19.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.15.1...v0.16.0
[0.15.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.2...v0.15.0
[0.14.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.1...v0.14.2
[0.14.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.2...v0.13.0
[0.12.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.11.1...v0.12.0
[0.11.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/noahsabaj/mermaid-cli/releases/tag/v0.1.0
