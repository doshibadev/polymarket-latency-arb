import { Config, CommandPayload, EMPTY_CONFIG } from "./types";

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
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(serializeConfig(config)),
    }),
  );
}

export async function sendCommand(payload: CommandPayload) {
  await ensureOk(
    await fetch("/command", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }),
  );
}
