import { FinishedSession, StartRequest, StatusSnapshot, TokenInfo } from "./types";

// In browser: call our own Next.js API routes (which proxy to the EC2 backend)
// The browser never talks directly to EC2.

async function apiFetch<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...options?.headers,
    },
  });

  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body?.error ?? `HTTP ${res.status}`);
  }

  const json = await res.json();
  if (json.success === false) throw new Error(json.error ?? "Unknown error");
  return json.data ?? json;
}

export const api = {
  tokens: (): Promise<TokenInfo[]> => apiFetch("/api/tokens"),

  status: (): Promise<StatusSnapshot | null> => apiFetch("/api/status"),

  start: (req: StartRequest): Promise<{ started: boolean; token: string }> =>
    apiFetch("/api/start", { method: "POST", body: JSON.stringify(req) }),

  stop: (): Promise<{ stopped: boolean }> =>
    apiFetch("/api/stop", { method: "POST" }),

  sessions: (): Promise<FinishedSession[]> => apiFetch("/api/sessions"),

  ec2Status: (): Promise<{ state: string }> => apiFetch("/api/ec2/status"),
  ec2Start: (): Promise<{ started: boolean }> => apiFetch("/api/ec2/start", { method: "POST" }),
  ec2Stop: (): Promise<{ stopped: boolean }> => apiFetch("/api/ec2/stop", { method: "POST" }),
};
