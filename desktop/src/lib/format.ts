export function shortId(value: string | null | undefined, width = 11): string {
  if (!value) return "none";
  if (value.length <= width) return value;
  return `${value.slice(0, width)}...`;
}

export function formatDate(value: string | null | undefined): string {
  if (!value) return "never";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(date);
}

export function humanPath(value: string | null | undefined): string {
  if (!value) return "none";
  const home = homePrefix();
  return home && value.startsWith(home) ? `~${value.slice(home.length)}` : value;
}

function homePrefix(): string | null {
  return null;
}

export function safeText(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

export function parseJson(raw: string | null | undefined): unknown {
  if (!raw) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

export function statusTone(status: string | null | undefined): string {
  switch (status) {
    case "running":
      return "tone-live";
    case "waiting_for_approval":
      return "tone-warn";
    case "blocked":
    case "failed":
      return "tone-danger";
    case "completed":
      return "tone-good";
    case "cancelled":
    case "exited":
      return "tone-muted";
    default:
      return "tone-neutral";
  }
}

export function decisionTone(value: string | null | undefined): string {
  switch (value) {
    case "deny":
    case "denied":
      return "tone-danger";
    case "ask":
    case "waiting_for_approval":
      return "tone-warn";
    case "allow":
    case "approved":
      return "tone-good";
    default:
      return "tone-neutral";
  }
}

export function riskTone(value: string | null | undefined): string {
  const normalized = value?.toLowerCase() ?? "";
  if (normalized.includes("high") || normalized.includes("destructive")) return "tone-danger";
  if (normalized.includes("medium") || normalized.includes("risky")) return "tone-warn";
  if (normalized.includes("low") || normalized.includes("safe")) return "tone-good";
  return "tone-neutral";
}

export function actionSummary(action: unknown, fallback = "No action details"): string {
  if (!action) return fallback;
  if (typeof action === "string") return action;
  if (typeof action !== "object") return String(action);
  const object = action as Record<string, unknown>;
  const kind = object.kind ?? object.type ?? object.tool ?? object.command;
  const target = object.path ?? object.file_path ?? object.cwd ?? object.project_path;
  if (kind && target) return `${kind}: ${target}`;
  if (kind) return String(kind);
  return JSON.stringify(action, null, 2);
}
