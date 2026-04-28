import { invoke } from "@tauri-apps/api/core";
import type {
  ApprovalDetail,
  CheckpointDetail,
  Dashboard,
  RuntimeHygieneArchive,
  RuntimeHygienePreview,
  TaskDetail,
} from "./types";

type Value = Record<string, unknown>;

export function dashboard(): Promise<Dashboard> {
  return invoke<Dashboard>("desktop_dashboard");
}

export function diagnostics(): Promise<Value> {
  return invoke<Value>("desktop_diagnostics");
}

export function hygienePreview(): Promise<RuntimeHygienePreview> {
  return invoke<RuntimeHygienePreview>("desktop_hygiene_preview");
}

export function hygieneArchive(): Promise<RuntimeHygieneArchive> {
  return invoke<RuntimeHygieneArchive>("desktop_hygiene_archive");
}

export function health(): Promise<Value> {
  return invoke<Value>("daemon_health");
}

export function daemonService(action: "start" | "stop" | "restart" | "status"): Promise<Value> {
  return invoke<Value>("daemon_service", { action });
}

export function taskDetail(id: string): Promise<TaskDetail> {
  return invoke<TaskDetail>("desktop_task_detail", { id });
}

export function approvalDetail(id: string): Promise<ApprovalDetail> {
  return invoke<ApprovalDetail>("desktop_approval_detail", { id });
}

export function checkpointDetail(id: string): Promise<CheckpointDetail> {
  return invoke<CheckpointDetail>("desktop_checkpoint_detail", { id });
}

export function runTask(prompt: string, projectPath: string, modelId: string): Promise<Value> {
  return invoke<Value>("run_task", {
    prompt,
    projectPath: projectPath || null,
    modelId: modelId || null,
  });
}

export function approve(id: string): Promise<Value> {
  return invoke<Value>("approve", { id });
}

export function deny(id: string): Promise<Value> {
  return invoke<Value>("deny", { id });
}

export function restoreCheckpoint(id: string): Promise<Value> {
  return invoke<Value>("restore_checkpoint", { id });
}

export function setSafetyMode(mode: string): Promise<Value> {
  return invoke<Value>("set_safety_mode", { mode });
}

export function processLogs(id: string, tailBytes = 32768): Promise<{ content: string }> {
  return invoke<{ content: string }>("process_logs", { id, tailBytes });
}

export function stopProcess(id: string): Promise<Value> {
  return invoke<Value>("stop_process", { id });
}

export function restartProcess(id: string): Promise<Value> {
  return invoke<Value>("restart_process", { id });
}

export function openProcess(id: string): Promise<Value> {
  return invoke<Value>("open_process", { id });
}

export function memoryEdit(id: string, value: string): Promise<Value> {
  return invoke<Value>("memory_edit", { id, value });
}

export function forget(id: string): Promise<Value> {
  return invoke<Value>("forget", { id });
}

export function pluginPreview(path: string): Promise<Value> {
  return invoke<Value>("plugin_preview", { path });
}

export function pluginInstall(path: string): Promise<Value> {
  return invoke<Value>("plugin_install", { path });
}

export function setPluginEnabled(id: string, enabled: boolean): Promise<Value> {
  return invoke<Value>("set_plugin_enabled", { id, enabled });
}

export function createPairing(label: string): Promise<{ token?: string; pairing?: Value }> {
  return invoke<{ token?: string; pairing?: Value }>("create_pairing", { label: label || null });
}
