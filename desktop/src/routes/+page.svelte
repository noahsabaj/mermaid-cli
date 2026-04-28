<script lang="ts">
  import { onMount } from "svelte";
  import {
    Bell,
    Brain,
    Check,
    CheckSquare,
    Clipboard,
    Database,
    ExternalLink,
    History,
    KeyRound,
    ListChecks,
    Mic,
    Monitor,
    Moon,
    Play,
    Plug,
    RefreshCw,
    RotateCcw,
    Save,
    ScrollText,
    Shield,
    Sun,
    TerminalSquare,
    X,
  } from "lucide-svelte";
  import * as daemon from "$lib/daemon";
  import {
    actionSummary,
    decisionTone,
    formatDate,
    humanPath,
    parseJson,
    riskTone,
    safeText,
    shortId,
    statusTone,
  } from "$lib/format";
  import type {
    ApprovalDetail,
    ApprovalRecord,
    CheckpointDetail,
    CheckpointRecord,
    Dashboard,
    MemoryEntry,
    ProcessRecord,
    ProviderProbe,
    RuntimeHygieneArchive,
    RuntimeHygienePreview,
    TaskDetail,
    TaskRecord,
  } from "$lib/types";

  type Section =
    | "attention"
    | "tasks"
    | "approvals"
    | "processes"
    | "checkpoints"
    | "models"
    | "memory"
    | "plugins"
    | "settings"
    | "diagnostics";
  type ThemePreference = "dark" | "light" | "system";

  const navItems: Array<{ id: Section; label: string; icon: typeof Bell }> = [
    { id: "attention", label: "Needs Attention", icon: Bell },
    { id: "tasks", label: "Tasks", icon: ListChecks },
    { id: "approvals", label: "Approvals", icon: CheckSquare },
    { id: "processes", label: "Processes", icon: TerminalSquare },
    { id: "checkpoints", label: "Checkpoints", icon: History },
    { id: "models", label: "Models", icon: Brain },
    { id: "memory", label: "Memory", icon: Database },
    { id: "plugins", label: "Plugins", icon: Plug },
    { id: "settings", label: "Settings", icon: Shield },
    { id: "diagnostics", label: "Diagnostics", icon: Clipboard },
  ];

  let active: Section = "attention";
  let dashboard: Dashboard | null = null;
  let diagnostics: Record<string, unknown> | null = null;
  let hygienePreview: RuntimeHygienePreview | null = null;
  let hygieneResult: RuntimeHygieneArchive | null = null;
  let loadError = "";
  let actionError = "";
  let actionNotice = "";
  let loading = false;
  let actionBusy = "";
  let lastRefresh = "";

  let taskPrompt = "";
  let taskProject = "";
  let taskModel = "";
  let selectedTaskId = "";
  let selectedTask: TaskDetail | null = null;

  let selectedApprovalId = "";
  let selectedApproval: ApprovalDetail | null = null;

  let selectedCheckpointId = "";
  let selectedCheckpoint: CheckpointDetail | null = null;

  let selectedProcessId = "";
  let processLog = "";

  let editingMemoryId = "";
  let memoryDraft = "";

  let pluginPath = "";
  let pluginPreview: Record<string, unknown> | null = null;

  let pairingLabel = "";
  let pairingToken = "";
  let serviceOutput = "";
  let themePreference: ThemePreference = "dark";
  let resolvedTheme: "dark" | "light" = "dark";
  let systemThemeQuery: MediaQueryList | null = null;

  $: tasks = dashboard?.tasks ?? [];
  $: approvals = dashboard?.approvals ?? [];
  $: processes = dashboard?.processes ?? [];
  $: checkpoints = dashboard?.checkpoints ?? [];
  $: memory = dashboard?.memory ?? [];
  $: plugins = dashboard?.plugins ?? [];
  $: providerProbes = dashboard?.provider_probes ?? [];
  $: toolRuns = dashboard?.tool_runs ?? [];
  $: counts = dashboard?.counts ?? {};
  $: runningTasks = tasks.filter((task) => task.status === "running");
  $: attentionTasks = tasks.filter((task) =>
    ["running", "waiting_for_approval", "blocked", "failed"].includes(task.status),
  );
  $: blockedOrFailedTasks = tasks.filter((task) => ["blocked", "failed"].includes(task.status));
  $: recentOutcomeTasks = tasks.filter((task) => ["completed", "failed", "cancelled"].includes(task.status));
  $: readyProcesses = processes.filter((process) => process.detected_url);
  $: homeSignalTotal = approvals.length + attentionTasks.length + readyProcesses.length;
  $: safetyMode = dashboard?.safety?.mode ?? "unknown";

  onMount(() => {
    initializeTheme();
    void refresh();
    return () => {
      systemThemeQuery?.removeEventListener("change", applyTheme);
    };
  });

  function initializeTheme() {
    const stored = window.localStorage.getItem("mermaid.desktop.theme");
    themePreference = isThemePreference(stored) ? stored : "dark";
    systemThemeQuery = window.matchMedia("(prefers-color-scheme: dark)");
    systemThemeQuery.addEventListener("change", applyTheme);
    applyTheme();
  }

  function isThemePreference(value: string | null): value is ThemePreference {
    return value === "dark" || value === "light" || value === "system";
  }

  function applyTheme() {
    resolvedTheme = themePreference === "system" ? (systemThemeQuery?.matches ? "dark" : "light") : themePreference;
    document.documentElement.dataset.theme = resolvedTheme;
    document.documentElement.style.colorScheme = resolvedTheme;
  }

  function setThemePreference(value: ThemePreference) {
    themePreference = value;
    window.localStorage.setItem("mermaid.desktop.theme", value);
    applyTheme();
  }

  function toggleTheme() {
    setThemePreference(resolvedTheme === "dark" ? "light" : "dark");
  }

  async function refresh() {
    loading = true;
    loadError = "";
    actionNotice = "";
    try {
      dashboard = await daemon.dashboard();
      lastRefresh = new Date().toLocaleTimeString();
      if (active === "diagnostics") {
        diagnostics = await daemon.diagnostics();
        hygienePreview = await daemon.hygienePreview();
      }
    } catch (error) {
      dashboard = null;
      loadError = errorMessage(error);
    } finally {
      loading = false;
    }
  }

  async function setActive(section: Section) {
    active = section;
    actionError = "";
    if (section === "diagnostics") {
      await loadDiagnostics();
    }
  }

  async function loadDiagnostics() {
    actionBusy = "diagnostics";
    try {
      diagnostics = await daemon.diagnostics();
      hygienePreview = await daemon.hygienePreview();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function previewHygiene() {
    actionBusy = "hygiene-preview";
    actionError = "";
    try {
      hygienePreview = await daemon.hygienePreview();
      diagnostics = await daemon.diagnostics();
      hygieneResult = null;
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function archiveHygiene() {
    actionBusy = "hygiene-archive";
    actionError = "";
    try {
      hygieneResult = await daemon.hygieneArchive();
      hygienePreview = await daemon.hygienePreview();
      diagnostics = await daemon.diagnostics();
      await refresh();
      actionNotice = "Test/dev runtime artifacts archived from primary views.";
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function runService(action: "start" | "stop" | "restart" | "status") {
    actionBusy = `service-${action}`;
    actionError = "";
    try {
      const result = await daemon.daemonService(action);
      serviceOutput = JSON.stringify(result, null, 2);
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function submitTask() {
    if (!taskPrompt.trim()) return;
    actionBusy = "run-task";
    actionError = "";
    try {
      await daemon.runTask(taskPrompt.trim(), taskProject.trim(), taskModel.trim());
      taskPrompt = "";
      actionNotice = "Task submitted to mermaidd.";
      await refresh();
      active = "tasks";
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function loadTask(id: string) {
    selectedTaskId = id;
    selectedTask = null;
    actionBusy = `task-${id}`;
    try {
      selectedTask = await daemon.taskDetail(id);
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function loadApproval(id: string) {
    selectedApprovalId = id;
    selectedApproval = null;
    actionBusy = `approval-${id}`;
    try {
      selectedApproval = await daemon.approvalDetail(id);
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function decideApproval(decision: "approve" | "deny") {
    const id = selectedApproval?.approval?.id;
    if (!id) return;
    actionBusy = `${decision}-${id}`;
    actionError = "";
    try {
      if (decision === "approve") {
        await daemon.approve(id);
      } else {
        await daemon.deny(id);
      }
      selectedApproval = null;
      selectedApprovalId = "";
      actionNotice = decision === "approve" ? "Approval accepted and replay requested." : "Approval denied.";
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function loadCheckpoint(id: string) {
    selectedCheckpointId = id;
    selectedCheckpoint = null;
    actionBusy = `checkpoint-${id}`;
    try {
      selectedCheckpoint = await daemon.checkpointDetail(id);
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function restoreSelectedCheckpoint() {
    const id = selectedCheckpoint?.checkpoint?.id;
    if (!id) return;
    actionBusy = `restore-${id}`;
    actionError = "";
    try {
      await daemon.restoreCheckpoint(id);
      actionNotice = "Checkpoint restored. Any replayable action is back in the approval inbox.";
      await refresh();
      await loadCheckpoint(id);
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function showProcessLogs(process: ProcessRecord) {
    selectedProcessId = process.id;
    processLog = "";
    actionBusy = `logs-${process.id}`;
    try {
      const result = await daemon.processLogs(process.id);
      processLog = result.content || "No log output.";
    } catch (error) {
      processLog = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function processAction(action: "open" | "restart" | "stop", process: ProcessRecord) {
    actionBusy = `${action}-${process.id}`;
    actionError = "";
    try {
      if (action === "open") await daemon.openProcess(process.id);
      if (action === "restart") await daemon.restartProcess(process.id);
      if (action === "stop") await daemon.stopProcess(process.id);
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  function beginMemoryEdit(entry: MemoryEntry) {
    editingMemoryId = entry.id;
    memoryDraft = entry.value;
  }

  async function saveMemory(entry: MemoryEntry) {
    actionBusy = `memory-${entry.id}`;
    actionError = "";
    try {
      await daemon.memoryEdit(entry.id, memoryDraft);
      editingMemoryId = "";
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function forgetMemory(entry: MemoryEntry) {
    actionBusy = `forget-${entry.id}`;
    actionError = "";
    try {
      await daemon.forget(entry.id);
      if (editingMemoryId === entry.id) editingMemoryId = "";
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function auditPlugin() {
    if (!pluginPath.trim()) return;
    actionBusy = "plugin-audit";
    actionError = "";
    try {
      pluginPreview = await daemon.pluginPreview(pluginPath.trim());
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function installPlugin() {
    if (!pluginPath.trim()) return;
    actionBusy = "plugin-install";
    actionError = "";
    try {
      pluginPreview = await daemon.pluginInstall(pluginPath.trim());
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function togglePlugin(id: string, enabled: boolean) {
    actionBusy = `plugin-${id}`;
    actionError = "";
    try {
      await daemon.setPluginEnabled(id, enabled);
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function updateSafety(mode: string) {
    actionBusy = `safety-${mode}`;
    actionError = "";
    try {
      await daemon.setSafetyMode(mode);
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  async function createPairing() {
    actionBusy = "pairing";
    actionError = "";
    try {
      const result = await daemon.createPairing(pairingLabel.trim());
      pairingToken = result.token ?? "";
      pairingLabel = "";
      await refresh();
    } catch (error) {
      actionError = errorMessage(error);
    } finally {
      actionBusy = "";
    }
  }

  function navCount(section: Section): number | null {
    switch (section) {
      case "attention":
        return (counts.pending_approvals ?? 0) + (counts.blocked_tasks ?? 0) + (counts.waiting_tasks ?? 0);
      case "tasks":
        return tasks.length;
      case "approvals":
        return approvals.length;
      case "processes":
        return processes.length;
      case "checkpoints":
        return checkpoints.length;
      case "models":
        return providerProbes.length;
      case "memory":
        return memory.length;
      case "plugins":
        return plugins.length;
      default:
        return null;
    }
  }

  function errorMessage(error: unknown): string {
    if (error instanceof Error) return error.message;
    if (typeof error === "string") return error;
    return safeText(error);
  }

  function messagePreview(content: string): string {
    const parsed = parseJson(content);
    const text = safeText(parsed).replace(/\s+/g, " ").trim();
    return text.length > 280 ? `${text.slice(0, 280)}...` : text;
  }

  function statusLabel(status: string | null | undefined): string {
    return (status ?? "unknown").replaceAll("_", " ");
  }

  function approvalTitle(approval: ApprovalRecord): string {
    return approval.proposed_action.replace(/^restore replay:\s*/i, "Replay ");
  }

  function approvalDetails(approval: ApprovalRecord): string {
    const parsed = parseJson(approval.pending_action_json ?? approval.args_summary ?? "");
    return actionSummary(parsed, approval.args_summary ?? approval.pending_action_json ?? approval.id);
  }

  function riskExplanation(risk: string): string {
    switch (risk) {
      case "restored_action":
        return "Replay of an action restored from a checkpoint.";
      case "file_mutation":
        return "May write, create, or modify files in the project.";
      case "shell":
      case "process":
        return "May start or control a local command or process.";
      case "network":
      case "web":
        return "May access network resources.";
      default:
        return "Requires review under the current safety policy.";
    }
  }

  function checkpointTone(id: string | null | undefined): string {
    return id ? "tone-good" : "tone-danger";
  }

  function checkpointText(id: string | null | undefined): string {
    return id ? "checkpoint captured" : "no checkpoint";
  }

  function processTitle(process: ProcessRecord): string {
    return process.command.split(/\s+/).slice(0, 4).join(" ") || shortId(process.id, 18);
  }

  function changedFilePaths(value: unknown): string[] {
    if (!Array.isArray(value)) return [];
    return value
      .map((entry) => {
        if (entry && typeof entry === "object" && "path" in entry) {
          const path = (entry as { path?: unknown }).path;
          return typeof path === "string" ? path : "";
        }
        return typeof entry === "string" ? entry : "";
      })
      .filter(Boolean);
  }

  function providerGroups(probes: ProviderProbe[]) {
    const groups = new Map<string, ProviderProbe[]>();
    for (const probe of probes) {
      const key = `${probe.provider} / ${probe.model_id}`;
      groups.set(key, [...(groups.get(key) ?? []), probe]);
    }
    return [...groups.entries()];
  }

  function arrayCount(value: unknown): number {
    return Array.isArray(value) ? value.length : 0;
  }

  function rawTableCount(name: string): number {
    return arrayCount(diagnostics?.[name]);
  }

  function hygieneArchivedCount(kind: "approvals" | "checkpoints"): number {
    const hygiene = diagnostics?.hygiene as { archived?: Record<string, unknown> } | undefined;
    const value = hygiene?.archived?.[kind];
    return typeof value === "number" ? value : 0;
  }
</script>

<svelte:head>
  <title>Mermaid Desktop</title>
</svelte:head>

<div class="app-shell">
  <aside class="sidebar">
    <div class="sidebar-title">Mermaid</div>
    <div class="mt-1 px-2 text-xs text-slate-400">Local operator console</div>
    <nav class="mt-6 space-y-1">
      {#each navItems as item (item.id)}
        {@const Icon = item.icon}
        <button
          class:nav-button-active={active === item.id}
          class="nav-button"
          type="button"
          onclick={() => setActive(item.id)}
        >
          <Icon size={16} />
          <span>{item.label}</span>
          {#if navCount(item.id) !== null}
            <span class="nav-count">{navCount(item.id)}</span>
          {/if}
        </button>
      {/each}
    </nav>
    <div class="mt-auto rounded-lg border border-white/10 bg-white/5 p-3 text-xs text-slate-400">
      <div class="mb-1 font-medium text-slate-200">Daemon</div>
      <div>{dashboard?.health?.service ?? "disconnected"}</div>
      <div class="mt-1 truncate">{dashboard?.health?.database ?? "runtime unavailable"}</div>
    </div>
  </aside>

  <main class="content">
    <header class="topbar">
      <div class="flex min-w-0 items-center gap-3">
        <span class={`chip ${loading ? "tone-warn" : dashboard ? "tone-good" : "tone-danger"}`}>
          {loading ? "Refreshing" : dashboard ? "Daemon attached" : "Daemon offline"}
        </span>
        <span class={`chip ${decisionTone(safetyMode)}`}>Safety: {safetyMode}</span>
        <span class="muted truncate">Last refresh {lastRefresh || "never"}</span>
      </div>
      <div class="flex items-center gap-2">
        <button
          class="button icon-button"
          type="button"
          title={resolvedTheme === "dark" ? "Switch to light theme" : "Switch to dark theme"}
          onclick={toggleTheme}
        >
          {#if resolvedTheme === "dark"}
            <Sun size={15} />
          {:else}
            <Moon size={15} />
          {/if}
        </button>
        <button class="button button-dark" type="button" onclick={refresh} disabled={loading}>
          <RefreshCw size={15} />
          Refresh
        </button>
      </div>
    </header>

    <section class="section">
        {#if actionError}
          <div class="mb-4 rounded-md border border-red-200 bg-red-50 p-3 text-sm text-red-700">{actionError}</div>
        {/if}
        {#if actionNotice}
          <div class="mb-4 rounded-md border border-emerald-200 bg-emerald-50 p-3 text-sm text-emerald-700">
            {actionNotice}
          </div>
        {/if}
        {#if loadError && !dashboard && active !== "attention"}
          <div class="offline-strip mb-4">
            <div>
              <div class="font-medium">Daemon offline</div>
              <div class="mt-1 text-sm">This section is still available. Runtime data and daemon actions will reconnect after <code>mermaidd</code> starts.</div>
            </div>
            <button class="button" type="button" onclick={() => setActive("attention")}>Recovery</button>
          </div>
        {/if}

        {#if active === "attention"}
          {#if loadError && !dashboard}
            {@render DaemonOfflinePanel()}
          {:else}
          <div class="section-heading">
            <div>
              <h1 class="h1">Needs Attention</h1>
              <p class="muted mt-1">Decisions, blocked work, and ready local processes.</p>
            </div>
            <button class="button button-primary" type="button" onclick={() => setActive("tasks")}>
              <Play size={15} />
              New task
            </button>
          </div>

          <div class="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
            <div class="metric">
              <div class="metric-label">Pending approvals</div>
              <div class="metric-value">{counts.pending_approvals ?? 0}</div>
              <div class="metric-note">actions waiting for trust review</div>
            </div>
            <div class="metric">
              <div class="metric-label">Running tasks</div>
              <div class="metric-value">{runningTasks.length}</div>
              <div class="metric-note">currently owned by the daemon</div>
            </div>
            <div class="metric">
              <div class="metric-label">Blocked or failed</div>
              <div class="metric-value">{blockedOrFailedTasks.length}</div>
              <div class="metric-note">needs inspection before retry</div>
            </div>
            <div class="metric">
              <div class="metric-label">Ready processes</div>
              <div class="metric-value">{counts.ready_processes ?? 0}</div>
              <div class="metric-note">reported a URL or service endpoint</div>
            </div>
          </div>

          {#if homeSignalTotal === 0}
            <div class="surface mt-4">
              <div class="surface-body flex flex-wrap items-center justify-between gap-3">
                <div>
                  <div class="font-semibold text-slate-950">No active decisions or blocked work.</div>
                  <div class="muted mt-1">The daemon is attached and the primary queue is clear.</div>
                </div>
                <div class="flex flex-wrap gap-2">
                  <button class="button button-primary" type="button" onclick={() => setActive("tasks")}>
                    <Play size={15} />
                    New task
                  </button>
                  <button class="button" type="button" onclick={() => setActive("diagnostics")}>Diagnostics</button>
                </div>
              </div>
            </div>
          {/if}

          <div class="mt-5 grid gap-4 xl:grid-cols-[1.1fr_0.9fr]">
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Approval Review Queue</h2>
                <span class="chip tone-warn">{approvals.length} pending</span>
              </div>
              <div class="divide-y divide-slate-200">
                {#each approvals.slice(0, 8) as approval (approval.id)}
                  <div class="record-row">
                    <div class="min-w-0">
                      <div class="flex flex-wrap items-center gap-2">
                        <span class="font-medium text-slate-950">{approvalTitle(approval)}</span>
                        <span class={`chip ${riskTone(approval.risk_classification)}`}>
                          {statusLabel(approval.risk_classification)}
                        </span>
                        <span class={`chip ${checkpointTone(approval.checkpoint_id)}`}>
                          {checkpointText(approval.checkpoint_id)}
                        </span>
                      </div>
                      <div class="muted mt-1 truncate">{approvalDetails(approval)}</div>
                    </div>
                    <button class="button" type="button" onclick={() => { active = "approvals"; void loadApproval(approval.id); }}>
                      Review
                    </button>
                  </div>
                {:else}
                  <div class="surface-body"><div class="empty empty-compact">No approval decisions are waiting.</div></div>
                {/each}
              </div>
            </div>

            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Active Work</h2>
                <span class="chip tone-neutral">{attentionTasks.length} tasks</span>
              </div>
              <div class="divide-y divide-slate-200">
                {#each attentionTasks.slice(0, 8) as task (task.id)}
                  <button class="list-item row-button w-full text-left" type="button" onclick={() => { active = "tasks"; void loadTask(task.id); }}>
                    <div class="flex items-center justify-between gap-3">
                      <span class="font-medium text-slate-950">{task.title}</span>
                      <span class={`chip ${statusTone(task.status)}`}>{statusLabel(task.status)}</span>
                    </div>
                    <div class="muted mt-1 truncate">{humanPath(task.project_path)} · {task.model_id}</div>
                    <div class="mt-1 text-xs text-slate-500">updated {formatDate(task.updated_at)}</div>
                  </button>
                {:else}
                  <div class="surface-body"><div class="empty empty-compact">No running or blocked tasks.</div></div>
                {/each}
              </div>
            </div>
          </div>

          <div class="mt-4 grid gap-4 xl:grid-cols-2">
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Ready Processes</h2>
                <span class="chip tone-live">{readyProcesses.length} with URL</span>
              </div>
              <div class="divide-y divide-slate-200">
                {#each readyProcesses.slice(0, 6) as process (process.id)}
                  <div class="record-row">
                    <div class="min-w-0">
                      <div class="font-medium text-slate-950">{processTitle(process)}</div>
                      <div class="muted truncate">{process.detected_url}</div>
                      <div class="mt-1 text-xs text-slate-500">pid {process.pid} · {statusLabel(process.status)}</div>
                    </div>
                    <button class="button" type="button" onclick={() => processAction("open", process)}>
                      <ExternalLink size={15} />
                      Open
                    </button>
                  </div>
                {:else}
                  <div class="surface-body"><div class="empty empty-compact">No ready process URLs.</div></div>
                {/each}
              </div>
            </div>

            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Recent Outcomes</h2>
                <button class="button button-ghost" type="button" onclick={() => setActive("tasks")}>View all</button>
              </div>
              <div class="divide-y divide-slate-200">
                {#each recentOutcomeTasks.slice(0, 6) as task (task.id)}
                  <button class="list-item row-button w-full text-left" type="button" onclick={() => { active = "tasks"; void loadTask(task.id); }}>
                    <div class="flex items-center justify-between gap-3">
                      <span class="font-medium text-slate-950">{task.title}</span>
                      <span class={`chip ${statusTone(task.status)}`}>{statusLabel(task.status)}</span>
                    </div>
                    <div class="muted mt-1 truncate">{humanPath(task.project_path)} · {formatDate(task.updated_at)}</div>
                  </button>
                {:else}
                  <div class="surface-body"><div class="empty empty-compact">No completed, failed, or cancelled tasks yet.</div></div>
                {/each}
              </div>
            </div>
          </div>
          {/if}
        {:else if active === "tasks"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Tasks</h1>
              <p class="muted mt-1">Create work for the daemon and inspect durable task state.</p>
            </div>
          </div>

          <div class="surface mb-4">
            <div class="surface-header">
              <div>
                <h2 class="h2">New Task</h2>
                <p class="muted mt-1">Queued work runs through the local daemon.</p>
              </div>
              <span class="chip tone-neutral">{safetyMode}</span>
            </div>
            <div class="surface-body grid gap-3">
              <label>
                <span class="field-label">Request</span>
                <textarea class="textarea" bind:value={taskPrompt} placeholder="Ask Mermaid to work on this machine..."></textarea>
              </label>
              <div class="grid gap-3 md:grid-cols-[minmax(0,1fr)_minmax(220px,0.35fr)]">
                <label>
                  <span class="field-label">Project path</span>
                  <input class="input" bind:value={taskProject} placeholder="Current directory when blank" />
                </label>
                <label>
                  <span class="field-label">Model</span>
                  <input class="input" bind:value={taskModel} placeholder="Default profile when blank" />
                </label>
              </div>
              <div class="flex flex-wrap items-center justify-between gap-3">
                <div class="muted">Tasks continue locally and can request approvals before risky actions.</div>
                <button class="button button-primary" type="button" onclick={submitTask} disabled={!taskPrompt.trim() || actionBusy === "run-task"}>
                  <Play size={15} />
                  Start Task
                </button>
              </div>
            </div>
          </div>

          <div class="detail-grid">
            <div class="list-panel">
              <div class="surface-header">
                <h2 class="h2">Task Queue</h2>
                <span class="chip tone-neutral">{tasks.length} tasks</span>
              </div>
              {#each tasks as task (task.id)}
                <button
                  class:list-item-active={selectedTaskId === task.id}
                  class="list-item row-button w-full text-left"
                  type="button"
                  onclick={() => loadTask(task.id)}
                >
                  <div class="flex items-start justify-between gap-3">
                    <span class="font-medium text-slate-950">{task.title}</span>
                    <span class={`chip ${statusTone(task.status)}`}>{statusLabel(task.status)}</span>
                  </div>
                  <div class="muted mt-1 truncate">{humanPath(task.project_path)}</div>
                  <div class="mt-1 text-xs text-slate-500">{task.model_id} · {formatDate(task.updated_at)}</div>
                </button>
              {:else}
                <div class="surface-body"><div class="empty empty-compact">No tasks recorded.</div></div>
              {/each}
            </div>

            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Work Report</h2>
                {#if selectedTask?.task}
                  <span class={`chip ${statusTone(selectedTask.task.status)}`}>{statusLabel(selectedTask.task.status)}</span>
                {/if}
              </div>
              <div class="surface-body">
                {#if selectedTask?.task}
                  <div class="space-y-4">
                    <div>
                      <h3 class="text-lg font-semibold text-slate-950">{selectedTask.task.title}</h3>
                      <p class="muted mt-1">{humanPath(selectedTask.task.project_path)} · {selectedTask.task.model_id}</p>
                    </div>
                    <div class="summary-grid">
                      <div class="summary-cell">
                        <div class="metric-label">Status</div>
                        <div class="summary-value">{statusLabel(selectedTask.task.status)}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Priority</div>
                        <div class="summary-value">{selectedTask.task.priority}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Updated</div>
                        <div class="summary-value">{formatDate(selectedTask.task.updated_at)}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Artifacts</div>
                        <div class="summary-value">
                          {(selectedTask.approvals?.length ?? 0) + (selectedTask.checkpoints?.length ?? 0) + (selectedTask.processes?.length ?? 0)}
                        </div>
                      </div>
                    </div>
                    <div>
                      <h3 class="mb-2 text-sm font-semibold text-slate-950">Timeline</h3>
                      <div class="rounded-md border border-slate-200">
                        {#each selectedTask.events ?? [] as event (event.id)}
                          <div class="kv">
                            <div class="kv-key">{formatDate(event.created_at)}</div>
                            <div class="kv-value"><span class="font-medium">{event.kind}</span> · {event.message}</div>
                          </div>
                        {:else}
                          <div class="p-3 text-sm text-slate-500">No timeline events.</div>
                        {/each}
                      </div>
                    </div>
                    {#if selectedTask.task.final_report}
                      <div>
                        <h3 class="mb-2 text-sm font-semibold text-slate-950">Final Report</h3>
                        <pre class="code">{selectedTask.task.final_report}</pre>
                      </div>
                    {/if}
                    <div class="grid gap-3 md:grid-cols-2">
                      {@render LinkedList("Approvals", selectedTask.approvals ?? [])}
                      {@render LinkedList("Checkpoints", selectedTask.checkpoints ?? [])}
                      {@render LinkedList("Processes", selectedTask.processes ?? [])}
                      {@render LinkedList("Tool Runs", selectedTask.tool_runs ?? [])}
                    </div>
                    <div>
                      <h3 class="mb-2 text-sm font-semibold text-slate-950">Messages</h3>
                      <div class="rounded-md border border-slate-200">
                        {#each selectedTask.messages ?? [] as message (message.id)}
                          <div class="border-b border-slate-100 p-3 last:border-b-0">
                            <div class="mb-1 text-xs font-semibold uppercase text-slate-500">{message.role}</div>
                            <div class="text-sm text-slate-700">{messagePreview(message.content_json)}</div>
                          </div>
                        {:else}
                          <div class="p-3 text-sm text-slate-500">No messages linked to this task.</div>
                        {/each}
                      </div>
                    </div>
                  </div>
                {:else}
                  <div class="empty empty-compact">Select a task to inspect its timeline, report, approvals, checkpoints, and tools.</div>
                {/if}
              </div>
            </div>
          </div>
        {:else if active === "approvals"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Approvals</h1>
              <p class="muted mt-1">Trust decisions with action context, affected paths, and checkpoint state.</p>
            </div>
          </div>

          <div class="detail-grid">
            <div class="list-panel">
              <div class="surface-header">
                <h2 class="h2">Review Queue</h2>
                <span class="chip tone-warn">{approvals.length} pending</span>
              </div>
              {#each approvals as approval (approval.id)}
                <button
                  class:list-item-active={selectedApprovalId === approval.id}
                  class="list-item row-button w-full text-left"
                  type="button"
                  onclick={() => loadApproval(approval.id)}
                >
                  <div class="flex items-start justify-between gap-3">
                    <span class="font-medium text-slate-950">{approvalTitle(approval)}</span>
                    <span class={`chip ${riskTone(approval.risk_classification)}`}>{statusLabel(approval.risk_classification)}</span>
                  </div>
                  <div class="muted mt-1 truncate">{approvalDetails(approval)}</div>
                  <div class="mt-2 flex flex-wrap gap-2">
                    <span class={`chip ${decisionTone(approval.policy_decision)}`}>{approval.policy_decision}</span>
                    <span class={`chip ${checkpointTone(approval.checkpoint_id)}`}>{checkpointText(approval.checkpoint_id)}</span>
                    <span class="chip tone-muted">{formatDate(approval.created_at)}</span>
                  </div>
                </button>
              {:else}
                <div class="surface-body"><div class="empty empty-compact">No pending approvals.</div></div>
              {/each}
            </div>

            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Decision Detail</h2>
                {#if selectedApproval?.approval}
                  <span class={`chip ${decisionTone(selectedApproval.approval.policy_decision)}`}>
                    {selectedApproval.approval.policy_decision}
                  </span>
                {/if}
              </div>
              <div class="surface-body">
                {#if selectedApproval?.approval}
                  {@const approval = selectedApproval.approval}
                  {@const affectedPaths = selectedApproval.affected_paths ?? []}
                  <div class="space-y-4">
                    <div class="decision-panel">
                      <div>
                        <div class="metric-label">Proposed action</div>
                        <div class="mt-1 text-lg font-semibold text-slate-950">{approvalTitle(approval)}</div>
                        <div class="muted mt-1">{approvalDetails(approval)}</div>
                      </div>
                      <div class="flex flex-wrap gap-2">
                        <span class={`chip ${riskTone(approval.risk_classification)}`}>{statusLabel(approval.risk_classification)}</span>
                        <span class={`chip ${checkpointTone(approval.checkpoint_id)}`}>{checkpointText(approval.checkpoint_id)}</span>
                      </div>
                    </div>
                    <div class="summary-grid">
                      <div class="summary-cell">
                        <div class="metric-label">Risk</div>
                        <div class="summary-value">{riskExplanation(approval.risk_classification)}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Task</div>
                        <div class="summary-value">{selectedApproval.task?.title ?? approval.task_id ?? "none"}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Checkpoint</div>
                        <div class="summary-value">{approval.checkpoint_id ? shortId(approval.checkpoint_id, 24) : "none"}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Created</div>
                        <div class="summary-value">{formatDate(approval.created_at)}</div>
                      </div>
                    </div>
                    <div>
                      <h3 class="mb-2 text-sm font-semibold text-slate-950">Affected Paths</h3>
                      {#if affectedPaths.length}
                        <div class="path-list">
                          {#each affectedPaths as path}
                            <div>{path}</div>
                          {/each}
                        </div>
                      {:else}
                        <div class="empty empty-compact">No affected paths were detected in the stored action payload.</div>
                      {/if}
                    </div>
                    <details class="disclosure">
                      <summary>Raw action payload and identifiers</summary>
                      <div class="mt-3 rounded-md border border-slate-200">
                        <div class="kv"><div class="kv-key">ID</div><div class="kv-value">{approval.id}</div></div>
                        <div class="kv"><div class="kv-key">Task ID</div><div class="kv-value">{approval.task_id ?? "none"}</div></div>
                        <div class="kv"><div class="kv-key">Checkpoint ID</div><div class="kv-value">{approval.checkpoint_id ?? "none"}</div></div>
                      </div>
                      <pre class="code mt-3">{safeText(selectedApproval.pending_action ?? approval.pending_action_json)}</pre>
                    </details>
                    <div class="flex flex-wrap justify-end gap-2">
                      <button class="button button-danger" type="button" onclick={() => decideApproval("deny")} disabled={actionBusy.startsWith("deny-")}>
                        <X size={15} />
                        Deny
                      </button>
                      <button class="button button-primary" type="button" onclick={() => decideApproval("approve")} disabled={actionBusy.startsWith("approve-")}>
                        <Check size={15} />
                        Approve and Replay
                      </button>
                    </div>
                  </div>
                {:else}
                  <div class="empty empty-compact">Select an approval to review the action, affected paths, and checkpoint.</div>
                {/if}
              </div>
            </div>
          </div>
        {:else if active === "processes"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Processes</h1>
              <p class="muted mt-1">Daemon-owned background processes and live log tails.</p>
            </div>
          </div>

          <div class="grid gap-4 xl:grid-cols-[minmax(520px,1fr)_minmax(460px,0.85fr)]">
            <div class="list-panel">
              <div class="surface-header">
                <h2 class="h2">Process Registry</h2>
                <span class="chip tone-neutral">{processes.length} tracked</span>
              </div>
              {#each processes as process (process.id)}
                <div class="list-item">
                  <div class="flex flex-wrap items-start justify-between gap-3">
                    <div class="min-w-0">
                      <div class="flex flex-wrap items-center gap-2">
                        <span class="font-medium text-slate-950">{processTitle(process)}</span>
                        <span class={`chip ${statusTone(process.status)}`}>{statusLabel(process.status)}</span>
                        {#if process.detected_url}
                          <span class="chip tone-live">ready</span>
                        {/if}
                      </div>
                      <div class="muted mt-1 truncate">{process.command}</div>
                      <div class="mt-1 text-xs text-slate-500">pid {process.pid} · {humanPath(process.cwd)} · {formatDate(process.updated_at)}</div>
                      {#if process.detected_url}
                        <div class="muted mt-1 truncate">{process.detected_url}</div>
                      {/if}
                    </div>
                    <div class="flex flex-wrap justify-end gap-1">
                      <button class="button" type="button" onclick={() => showProcessLogs(process)}>Logs</button>
                      <button class="button" type="button" onclick={() => processAction("open", process)} disabled={!process.detected_url && !process.log_path}>
                        <ExternalLink size={15} />
                        Open
                      </button>
                      <button class="button" type="button" onclick={() => processAction("restart", process)}>Restart</button>
                      <button class="button button-danger" type="button" onclick={() => processAction("stop", process)}>Stop</button>
                    </div>
                  </div>
                </div>
              {:else}
                <div class="surface-body"><div class="empty empty-compact">No daemon-managed processes.</div></div>
              {/each}
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Log Tail</h2>
                <span class="chip tone-neutral">{selectedProcessId ? shortId(selectedProcessId, 18) : "none selected"}</span>
              </div>
              <div class="surface-body">
                <pre class="code min-h-[420px]">{processLog || "Select a process with logs."}</pre>
              </div>
            </div>
          </div>
        {:else if active === "checkpoints"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Checkpoints</h1>
              <p class="muted mt-1">Inspect restore points before reverting local files.</p>
            </div>
          </div>

          <div class="detail-grid">
            <div class="list-panel">
              <div class="surface-header">
                <h2 class="h2">Restore Points</h2>
                <span class="chip tone-neutral">{checkpoints.length} active</span>
              </div>
              {#each checkpoints as checkpoint (checkpoint.id)}
                <button
                  class:list-item-active={selectedCheckpointId === checkpoint.id}
                  class="list-item row-button w-full text-left"
                  type="button"
                  onclick={() => loadCheckpoint(checkpoint.id)}
                >
                  <div class="flex items-start justify-between gap-3">
                    <span class="font-medium text-slate-950">{shortId(checkpoint.id, 22)}</span>
                    <span class="muted">{formatDate(checkpoint.created_at)}</span>
                  </div>
                  <div class="muted mt-1 truncate">{humanPath(checkpoint.project_path)}</div>
                  <div class="mt-2 flex flex-wrap gap-2">
                    <span class={`chip ${checkpoint.approval_id ? "tone-good" : "tone-neutral"}`}>{checkpoint.approval_id ? "approval linked" : "manual checkpoint"}</span>
                    <span class="chip tone-muted">{shortId(checkpoint.snapshot_path, 28)}</span>
                  </div>
                </button>
              {:else}
                <div class="surface-body"><div class="empty empty-compact">No checkpoints recorded.</div></div>
              {/each}
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Restore Preview</h2>
                {#if selectedCheckpoint?.checkpoint}
                  <button class="button button-danger" type="button" onclick={restoreSelectedCheckpoint} disabled={actionBusy.startsWith("restore-")}>
                    <RotateCcw size={15} />
                    Restore
                  </button>
                {/if}
              </div>
              <div class="surface-body">
                {#if selectedCheckpoint?.checkpoint}
                  {@const paths = changedFilePaths(selectedCheckpoint.changed_files)}
                  <div class="space-y-4">
                    <div class="decision-panel">
                      <div>
                        <div class="metric-label">Project</div>
                        <div class="mt-1 text-lg font-semibold text-slate-950">{humanPath(selectedCheckpoint.checkpoint.project_path)}</div>
                        <div class="muted mt-1">{formatDate(selectedCheckpoint.checkpoint.created_at)}</div>
                      </div>
                      <span class={`chip ${selectedCheckpoint.checkpoint.approval_id ? "tone-good" : "tone-neutral"}`}>
                        {selectedCheckpoint.checkpoint.approval_id ? "approval linked" : "manual checkpoint"}
                      </span>
                    </div>
                    <div class="summary-grid">
                      <div class="summary-cell">
                        <div class="metric-label">Changed files</div>
                        <div class="summary-value">{paths.length}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Snapshot</div>
                        <div class="summary-value">{humanPath(selectedCheckpoint.checkpoint.snapshot_path)}</div>
                      </div>
                      <div class="summary-cell">
                        <div class="metric-label">Approval</div>
                        <div class="summary-value">{selectedCheckpoint.checkpoint.approval_id ?? "none"}</div>
                      </div>
                    </div>
                    <div>
                      <h3 class="mb-2 text-sm font-semibold text-slate-950">Changed Files</h3>
                      {#if paths.length}
                        <div class="path-list">
                          {#each paths as path}
                            <div>{path}</div>
                          {/each}
                        </div>
                      {:else}
                        <div class="empty empty-compact">No changed file paths were decoded from this checkpoint.</div>
                      {/if}
                    </div>
                    <details class="disclosure">
                      <summary>Raw checkpoint manifest</summary>
                      <div class="mt-3 rounded-md border border-slate-200">
                        <div class="kv"><div class="kv-key">ID</div><div class="kv-value">{selectedCheckpoint.checkpoint.id}</div></div>
                        <div class="kv"><div class="kv-key">Snapshot</div><div class="kv-value">{humanPath(selectedCheckpoint.checkpoint.snapshot_path)}</div></div>
                      </div>
                      <pre class="code mt-3">{safeText(selectedCheckpoint.changed_files)}</pre>
                      <pre class="code mt-3">{safeText(selectedCheckpoint.pending_action)}</pre>
                    </details>
                  </div>
                {:else}
                  <div class="empty empty-compact">Select a checkpoint to preview changed files and restore context.</div>
                {/if}
              </div>
            </div>
          </div>
        {:else if active === "models"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Model Reality</h1>
              <p class="muted mt-1">Cached provider capability records from static defaults and probes.</p>
            </div>
          </div>
          <div class="grid gap-4 xl:grid-cols-2">
            {#each providerGroups(providerProbes) as [model, probes] (model)}
              <div class="surface">
                <div class="surface-header">
                  <h2 class="h2">{model}</h2>
                  <span class={`chip ${probes.some((probe) => probe.confidence === "failed") ? "tone-danger" : "tone-neutral"}`}>
                    {probes.length} capabilities
                  </span>
                </div>
                <div class="surface-body">
                  <div class="rounded-md border border-slate-200">
                    {#each probes as probe (`${probe.provider}-${probe.model_id}-${probe.capability_key}`)}
                      <div class="kv">
                        <div class="kv-key">{probe.capability_key}</div>
                        <div class="kv-value">
                          {probe.capability_value}
                          <span class={`chip ml-2 ${probe.confidence === "failed" ? "tone-danger" : "tone-neutral"}`}>
                            {probe.confidence}
                          </span>
                          {#if probe.error}
                            <div class="mt-1 text-xs text-red-600">{probe.error}</div>
                          {/if}
                        </div>
                      </div>
                    {/each}
                  </div>
                </div>
              </div>
            {:else}
              <div class="empty">No provider probes have been recorded.</div>
            {/each}
          </div>
        {:else if active === "memory"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Memory</h1>
              <p class="muted mt-1">Auditable local project and global memory entries.</p>
            </div>
          </div>
          <div class="grid gap-3">
            {#each memory as entry (entry.id)}
              <div class="surface">
                <div class="surface-header">
                  <div class="min-w-0">
                    <h2 class="h2">{entry.key}</h2>
                    <p class="muted mt-1 truncate">{entry.scope} · {humanPath(entry.project_path)} · {entry.source}</p>
                  </div>
                  <div class="flex gap-2">
                    {#if editingMemoryId === entry.id}
                      <button class="button button-primary" type="button" onclick={() => saveMemory(entry)}>
                        <Save size={15} />
                        Save
                      </button>
                    {:else}
                      <button class="button" type="button" onclick={() => beginMemoryEdit(entry)}>Edit</button>
                    {/if}
                    <button class="button button-danger" type="button" onclick={() => forgetMemory(entry)}>Forget</button>
                  </div>
                </div>
                <div class="surface-body">
                  {#if editingMemoryId === entry.id}
                    <textarea class="textarea min-h-36 w-full" bind:value={memoryDraft}></textarea>
                  {:else}
                    <div class="whitespace-pre-wrap text-sm text-slate-700">{entry.value}</div>
                  {/if}
                </div>
              </div>
            {:else}
              <div class="empty">No memory entries are visible.</div>
            {/each}
          </div>
        {:else if active === "plugins"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Plugins</h1>
              <p class="muted mt-1">Install local plugin bundles after reviewing permissions.</p>
            </div>
          </div>
          <div class="surface mb-4">
            <div class="surface-header">
              <h2 class="h2">Install Bundle</h2>
              <span class="chip tone-warn">review first</span>
            </div>
            <div class="surface-body">
              <div class="grid gap-3 md:grid-cols-[minmax(0,1fr)_auto_auto]">
                <input class="input" bind:value={pluginPath} placeholder="Local plugin path" />
                <button class="button" type="button" onclick={auditPlugin}>
                  <ScrollText size={15} />
                  Audit
                </button>
                <button class="button button-primary" type="button" onclick={installPlugin}>
                  <Plug size={15} />
                  Install
                </button>
              </div>
              {#if pluginPreview}
                <pre class="code mt-3">{safeText(pluginPreview)}</pre>
              {/if}
            </div>
          </div>
          <div class="table-wrap">
            <table class="table">
              <thead>
                <tr><th>Name</th><th>Source</th><th>Version</th><th>Enabled</th><th>Actions</th></tr>
              </thead>
              <tbody>
                {#each plugins as plugin (plugin.id)}
                  <tr>
                    <td class="font-medium text-slate-950">{plugin.name}</td>
                    <td>{plugin.source}</td>
                    <td>{plugin.version ?? "none"}</td>
                    <td><span class={`chip ${plugin.enabled ? "tone-good" : "tone-muted"}`}>{plugin.enabled ? "enabled" : "disabled"}</span></td>
                    <td>
                      <button class="button" type="button" onclick={() => togglePlugin(plugin.id, !plugin.enabled)}>
                        {plugin.enabled ? "Disable" : "Enable"}
                      </button>
                    </td>
                  </tr>
                {:else}
                  <tr><td colspan="5"><div class="empty">No plugins installed.</div></td></tr>
                {/each}
              </tbody>
            </table>
          </div>
        {:else if active === "settings"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Settings</h1>
              <p class="muted mt-1">Safety mode, daemon service control, remote pairing, and experimental voice controls.</p>
            </div>
          </div>
          <div class="grid gap-4 xl:grid-cols-2">
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Appearance</h2>
                <span class="chip tone-neutral">Theme: {resolvedTheme}</span>
              </div>
              <div class="surface-body">
                <div class="segmented">
                  <button
                    class:segmented-active={themePreference === "dark"}
                    type="button"
                    onclick={() => setThemePreference("dark")}
                  >
                    <Moon size={15} />
                    Dark
                  </button>
                  <button
                    class:segmented-active={themePreference === "light"}
                    type="button"
                    onclick={() => setThemePreference("light")}
                  >
                    <Sun size={15} />
                    Light
                  </button>
                  <button
                    class:segmented-active={themePreference === "system"}
                    type="button"
                    onclick={() => setThemePreference("system")}
                  >
                    <Monitor size={15} />
                    System
                  </button>
                </div>
                <p class="muted mt-3">Mermaid starts in dark mode unless you choose light or system.</p>
              </div>
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Safety Mode</h2>
                <span class={`chip ${decisionTone(safetyMode)}`}>{safetyMode}</span>
              </div>
              <div class="surface-body flex flex-wrap gap-2">
                {#each ["read_only", "ask", "auto_review", "full_access"] as mode}
                  <button
                    class:button-primary={safetyMode === mode || safetyMode === mode.replace("_", "-")}
                    class="button"
                    type="button"
                    onclick={() => updateSafety(mode)}
                  >
                    {mode.replace("_", " ")}
                  </button>
                {/each}
              </div>
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Daemon Service</h2>
                <span class="chip tone-neutral">systemd user</span>
              </div>
              <div class="surface-body">
                <div class="flex flex-wrap gap-2">
                  <button class="button" type="button" onclick={() => runService("start")}>Start</button>
                  <button class="button" type="button" onclick={() => runService("restart")}>Restart</button>
                  <button class="button" type="button" onclick={() => runService("status")}>Status</button>
                  <button class="button button-danger" type="button" onclick={() => runService("stop")}>Stop</button>
                </div>
                {#if serviceOutput}
                  <pre class="code mt-3">{serviceOutput}</pre>
                {/if}
              </div>
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Remote Pairing</h2>
                <KeyRound size={16} />
              </div>
              <div class="surface-body">
                <div class="grid gap-3 md:grid-cols-[minmax(0,1fr)_auto]">
                  <input class="input" bind:value={pairingLabel} placeholder="Pairing label" />
                  <button class="button button-primary" type="button" onclick={createPairing}>Create token</button>
                </div>
                {#if pairingToken}
                  <div class="mt-3 rounded-md border border-amber-200 bg-amber-50 p-3 font-mono text-xs text-amber-900">
                    {pairingToken}
                  </div>
                {/if}
                <div class="mt-3 rounded-md border border-slate-200">
                  {#each dashboard?.pairings ?? [] as pairing (pairing.id)}
                    <div class="kv">
                      <div class="kv-key">{pairing.label ?? "Pairing"}</div>
                      <div class="kv-value">{shortId(pairing.id, 18)} · last used {formatDate(pairing.last_used_at)}</div>
                    </div>
                  {:else}
                    <div class="p-3 text-sm text-slate-500">No remote pairings.</div>
                  {/each}
                </div>
              </div>
            </div>
            <div class="surface">
              <div class="surface-header">
                <h2 class="h2">Voice</h2>
                <Mic size={16} />
              </div>
              <div class="surface-body">
                <div class="rounded-md border border-slate-200 bg-slate-50 p-3 text-sm text-slate-600">
                  Voice input and text-to-speech remain behind feature flags. This control surface is intentionally
                  secondary until transcription, secret filtering, and notification policies are production-hardened.
                </div>
              </div>
            </div>
          </div>
        {:else if active === "diagnostics"}
          <div class="section-heading">
            <div>
              <h1 class="h1">Diagnostics</h1>
              <p class="muted mt-1">Raw daemon snapshot for debugging runtime storage and UI mapping.</p>
            </div>
            <button class="button" type="button" onclick={loadDiagnostics}>
              <RefreshCw size={15} />
              Reload
            </button>
          </div>
          {#if loadError && !dashboard}
            <div class="mb-4 rounded-md border border-red-200 bg-red-50 p-3 text-sm text-red-700">
              Current daemon connection error: {loadError}
            </div>
          {/if}
          <div class="grid gap-4 xl:grid-cols-[0.8fr_1.2fr]">
            <div class="grid gap-4">
              <div class="surface">
                <div class="surface-header"><h2 class="h2">Runtime Tables</h2></div>
                <div class="surface-body grid gap-2">
                  <div class="kv"><div class="kv-key">Tasks</div><div class="kv-value">{rawTableCount("tasks") || tasks.length}</div></div>
                  <div class="kv"><div class="kv-key">Visible approvals</div><div class="kv-value">{approvals.length}</div></div>
                  <div class="kv"><div class="kv-key">Raw approvals</div><div class="kv-value">{rawTableCount("approvals")}</div></div>
                  <div class="kv"><div class="kv-key">Archived approvals</div><div class="kv-value">{hygieneArchivedCount("approvals")}</div></div>
                  <div class="kv"><div class="kv-key">Processes</div><div class="kv-value">{rawTableCount("processes") || processes.length}</div></div>
                  <div class="kv"><div class="kv-key">Tool runs</div><div class="kv-value">{rawTableCount("tool_runs") || toolRuns.length}</div></div>
                  <div class="kv"><div class="kv-key">Visible checkpoints</div><div class="kv-value">{checkpoints.length}</div></div>
                  <div class="kv"><div class="kv-key">Raw checkpoints</div><div class="kv-value">{rawTableCount("checkpoints")}</div></div>
                  <div class="kv"><div class="kv-key">Archived checkpoints</div><div class="kv-value">{hygieneArchivedCount("checkpoints")}</div></div>
                  <div class="kv"><div class="kv-key">Memory</div><div class="kv-value">{rawTableCount("memory") || memory.length}</div></div>
                </div>
              </div>
              <div class="surface">
                <div class="surface-header">
                  <div>
                    <h2 class="h2">Runtime Hygiene</h2>
                    <p class="muted mt-1">Archive test/dev artifacts from primary operator views without deleting records.</p>
                  </div>
                  <span class="chip tone-live">{hygienePreview?.counts?.total ?? 0} candidates</span>
                </div>
                <div class="surface-body grid gap-3">
                  <div class="grid gap-2 sm:grid-cols-3">
                    <div class="metric compact">
                      <div class="metric-label">Approvals</div>
                      <div class="metric-value">{hygienePreview?.counts?.approvals ?? 0}</div>
                    </div>
                    <div class="metric compact">
                      <div class="metric-label">Checkpoints</div>
                      <div class="metric-value">{hygienePreview?.counts?.checkpoints ?? 0}</div>
                    </div>
                    <div class="metric compact">
                      <div class="metric-label">Archived</div>
                      <div class="metric-value">{hygieneArchivedCount("approvals") + hygieneArchivedCount("checkpoints")}</div>
                    </div>
                  </div>
                  <div class="flex flex-wrap gap-2">
                    <button class="button" type="button" onclick={previewHygiene} disabled={actionBusy === "hygiene-preview"}>
                      <RefreshCw size={15} />
                      Preview
                    </button>
                    <button
                      class="button button-primary"
                      type="button"
                      onclick={archiveHygiene}
                      disabled={(hygienePreview?.counts?.total ?? 0) === 0 || actionBusy === "hygiene-archive"}
                    >
                      <Save size={15} />
                      Archive candidates
                    </button>
                  </div>
                  <div class="empty">
                    Matches checkpoints under <code>/tmp/mermaid_*</code> and restore-replay approvals linked to those checkpoints.
                    Archived records remain visible in Diagnostics and are retained in SQLite.
                  </div>
                  {#if hygieneResult}
                    <pre class="code">{safeText(hygieneResult)}</pre>
                  {/if}
                </div>
              </div>
            </div>
            <div class="surface">
              <div class="surface-header"><h2 class="h2">Raw Snapshot</h2></div>
              <div class="surface-body">
                <pre class="code min-h-[620px]">{safeText(diagnostics ?? dashboard)}</pre>
              </div>
            </div>
          </div>
        {/if}
    </section>
  </main>
</div>

{#snippet DaemonOfflinePanel()}
  <div class="surface">
    <div class="surface-header">
      <div>
        <h1 class="h1">Mermaid daemon is not reachable</h1>
        <p class="muted mt-1">
          The desktop app attaches to <code>mermaidd</code>, the local background service that owns tasks, approvals,
          checkpoints, processes, memory, plugins, and remote pairing.
        </p>
      </div>
    </div>
    <div class="surface-body space-y-4">
      <div class="rounded-md border border-red-200 bg-red-50 p-3 text-sm text-red-700">{loadError}</div>
      <div class="flex flex-wrap gap-2">
        <button class="button button-primary" type="button" onclick={() => runService("start")}>
          <Play size={15} />
          Start daemon
        </button>
        <button class="button" type="button" onclick={() => runService("restart")}>
          <RotateCcw size={15} />
          Restart
        </button>
        <button class="button" type="button" onclick={() => runService("status")}>
          <ScrollText size={15} />
          Status
        </button>
        <button class="button" type="button" onclick={refresh}>
          <RefreshCw size={15} />
          Retry attach
        </button>
      </div>
      <div class="rounded-md border border-slate-200 bg-slate-50 p-3 text-sm text-slate-700">
        If the user service has not been installed yet, run <code>mermaid daemon install --start</code> from a terminal,
        then refresh this window.
      </div>
      {#if serviceOutput}
        <pre class="code">{serviceOutput}</pre>
      {/if}
    </div>
  </div>
{/snippet}

<!-- Small local repeated record renderer. -->
{#snippet LinkedList(title: string, records: Array<{ id?: string; status?: string; tool_name?: string; proposed_action?: string; project_path?: string }>)}
  <div class="rounded-md border border-slate-200">
    <div class="border-b border-slate-200 bg-slate-50 px-3 py-2 text-xs font-semibold uppercase text-slate-500">{title}</div>
    {#each records as record, index (`${title}-${record.id ?? index}`)}
      <div class="border-b border-slate-100 px-3 py-2 text-sm last:border-b-0">
        <div class="font-medium text-slate-800">{record.proposed_action ?? record.tool_name ?? shortId(record.id, 18)}</div>
        {#if record.status}
          <span class={`chip mt-1 ${statusTone(record.status)}`}>{record.status}</span>
        {/if}
        {#if record.project_path}
          <div class="muted mt-1 truncate">{humanPath(record.project_path)}</div>
        {/if}
      </div>
    {:else}
      <div class="px-3 py-2 text-sm text-slate-500">None.</div>
    {/each}
  </div>
{/snippet}
