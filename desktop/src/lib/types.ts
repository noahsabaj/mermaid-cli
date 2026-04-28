export type JsonRecord = Record<string, unknown>;

export type Dashboard = {
  ok?: boolean;
  health?: {
    service?: string;
    database?: string;
  };
  safety?: {
    mode?: string;
  };
  counts?: {
    pending_approvals?: number;
    running_tasks?: number;
    waiting_tasks?: number;
    blocked_tasks?: number;
    ready_processes?: number;
    recent_checkpoints?: number;
    installed_plugins?: number;
    memory_entries?: number;
    archived_approvals?: number;
    archived_checkpoints?: number;
  };
  sessions?: SessionRecord[];
  tasks?: TaskRecord[];
  tool_runs?: ToolRunRecord[];
  processes?: ProcessRecord[];
  approvals?: ApprovalRecord[];
  checkpoints?: CheckpointRecord[];
  compactions?: CompactionRecord[];
  memory?: MemoryEntry[];
  plugins?: PluginInstall[];
  provider_probes?: ProviderProbe[];
  pairings?: PairingToken[];
};

export type TaskRecord = {
  id: string;
  title: string;
  status: string;
  priority: string;
  project_path: string;
  model_id: string;
  conversation_id?: string | null;
  created_at: string;
  updated_at: string;
  final_report?: string | null;
};

export type TaskEvent = {
  id: number;
  task_id: string;
  kind: string;
  message: string;
  created_at: string;
};

export type SessionRecord = {
  id: string;
  project_path: string;
  model_id: string;
  title?: string | null;
  conversation_path?: string | null;
  created_at: string;
  updated_at: string;
  total_tokens?: number | null;
};

export type MessageRecord = {
  id: number;
  session_id: string;
  role: string;
  content_json: string;
  created_at: string;
};

export type ApprovalRecord = {
  id: string;
  task_id?: string | null;
  proposed_action: string;
  risk_classification: string;
  policy_decision: string;
  user_decision?: string | null;
  args_summary?: string | null;
  checkpoint_id?: string | null;
  pending_action_json?: string | null;
  created_at: string;
  decided_at?: string | null;
  archived_at?: string | null;
  archive_reason?: string | null;
};

export type ProcessRecord = {
  id: string;
  task_id?: string | null;
  pid: number;
  command: string;
  cwd?: string | null;
  log_path?: string | null;
  detected_url?: string | null;
  status: string;
  health?: string | null;
  created_at: string;
  updated_at: string;
};

export type ToolRunRecord = {
  id: string;
  task_id?: string | null;
  turn_id?: string | null;
  call_id?: string | null;
  tool_name: string;
  status: string;
  args_json?: string | null;
  output_json?: string | null;
  started_at: string;
  finished_at?: string | null;
};

export type CheckpointRecord = {
  id: string;
  task_id?: string | null;
  project_path: string;
  snapshot_path: string;
  changed_files_json?: string;
  pending_action_json?: string | null;
  approval_id?: string | null;
  created_at: string;
  archived_at?: string | null;
  archive_reason?: string | null;
};

export type CompactionRecord = {
  id: string;
  task_id?: string | null;
  session_id?: string | null;
  source_token_estimate?: number | null;
  summary_token_count?: number | null;
  preserved_turns?: number | null;
  archive_path?: string | null;
  verification_status?: string | null;
  created_at: string;
};

export type MemoryEntry = {
  id: string;
  project_path?: string | null;
  scope: string;
  key: string;
  value: string;
  source: string;
  created_at: string;
  updated_at: string;
  deleted_at?: string | null;
};

export type PluginInstall = {
  id: string;
  name: string;
  source: string;
  version?: string | null;
  enabled: boolean;
  manifest_json: string;
  installed_at: string;
  updated_at: string;
};

export type ProviderProbe = {
  provider: string;
  model_id: string;
  capability_key: string;
  capability_value: string;
  confidence: string;
  error?: string | null;
  probed_at: string;
};

export type PairingToken = {
  id: string;
  token_hash: string;
  label?: string | null;
  enabled: boolean;
  created_at: string;
  last_used_at?: string | null;
};

export type TaskDetail = {
  task?: TaskRecord;
  events?: TaskEvent[];
  session?: SessionRecord | null;
  messages?: MessageRecord[];
  approvals?: ApprovalRecord[];
  checkpoints?: CheckpointRecord[];
  processes?: ProcessRecord[];
  tool_runs?: ToolRunRecord[];
  compactions?: CompactionRecord[];
};

export type ApprovalDetail = {
  approval?: ApprovalRecord;
  task?: TaskRecord | null;
  checkpoint?: CheckpointRecord | null;
  pending_action?: unknown;
  args?: unknown;
  changed_files?: unknown;
  affected_paths?: string[];
};

export type CheckpointDetail = {
  checkpoint?: CheckpointRecord;
  task?: TaskRecord | null;
  approval?: ApprovalRecord | null;
  pending_action?: unknown;
  changed_files?: unknown;
  affected_paths?: string[];
};

export type RuntimeHygienePreview = {
  ok?: boolean;
  reason?: string;
  approvals?: ApprovalRecord[];
  checkpoints?: CheckpointRecord[];
  counts?: {
    approvals?: number;
    checkpoints?: number;
    total?: number;
  };
};

export type RuntimeHygieneArchive = {
  ok?: boolean;
  reason?: string;
  archived?: {
    approvals?: number;
    checkpoints?: number;
    total?: number;
  };
  matched?: {
    approvals?: number;
    checkpoints?: number;
    total?: number;
  };
};
