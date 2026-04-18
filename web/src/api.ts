import { Config, CommandPayload, EMPTY_CONFIG } from "./types";

const AUTH_TOKEN_STORAGE_KEY = "lattice.dashboardAuthToken";

function readTokenFromUrl() {
  const url = new URL(window.location.href);
  const token = url.searchParams.get("token")?.trim();
  if (!token) return null;
  window.localStorage.setItem(AUTH_TOKEN_STORAGE_KEY, token);
  url.searchParams.delete("token");
  window.history.replaceState({}, "", url.toString());
  return token;
}

export function getDashboardAuthToken() {
  return (
    readTokenFromUrl() ??
    window.localStorage.getItem(AUTH_TOKEN_STORAGE_KEY)?.trim() ??
    null
  );
}

function buildHeaders() {
  const headers = new Headers({ "Content-Type": "application/json" });
  const token = getDashboardAuthToken();
  if (token) {
    headers.set("Authorization", `Bearer ${token}`);
  }
  return headers;
}

export function normalizeConfig(config: Partial<Config>): Config {
  const merged = { ...EMPTY_CONFIG, ...config };
  return {
    ...merged,
    portfolio_pct: merged.portfolio_pct * 100,
    max_drawdown_pct: merged.max_drawdown_pct * 100,
    early_exit_loss_pct: merged.early_exit_loss_pct * 100,
  };
}

export function serializeConfig(config: Config): Config {
  return {
    ...config,
    portfolio_pct: config.portfolio_pct / 100,
    max_drawdown_pct: config.max_drawdown_pct / 100,
    early_exit_loss_pct: config.early_exit_loss_pct / 100,
  };
}

async function ensureOk(response: Response) {
  if (!response.ok) {
    throw new Error(`Request failed with ${response.status}`);
  }
  return response;
}

export async function fetchConfig() {
  const response = await ensureOk(await fetch("/config"));
  const data = (await response.json()) as Partial<Config>;
  return normalizeConfig(data);
}

export async function saveConfig(config: Config) {
  await ensureOk(
    await fetch("/settings", {
      method: "POST",
      headers: buildHeaders(),
      body: JSON.stringify(serializeConfig(config)),
    }),
  );
}

export async function sendCommand(payload: CommandPayload) {
  await ensureOk(
    await fetch("/command", {
      method: "POST",
      headers: buildHeaders(),
      body: JSON.stringify(payload),
    }),
  );
}
