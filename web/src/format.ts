export function formatUsd(value: number, digits = 2) {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: digits,
    minimumFractionDigits: digits,
  }).format(value);
}

export function formatSignedUsd(value: number, digits = 2) {
  return `${value >= 0 ? "+" : "-"}${formatUsd(Math.abs(value), digits)}`;
}

export function formatPercent(value: number, digits = 2) {
  return `${value.toFixed(digits)}%`;
}

export function formatSignedPercent(value: number, digits = 2) {
  return `${value >= 0 ? "+" : ""}${value.toFixed(digits)}%`;
}

export function formatSharePrice(value: number) {
  return `${(value * 100).toFixed(1)}c`;
}

export function formatCompactNumber(value: number, digits = 2) {
  return new Intl.NumberFormat("en-US", {
    notation: "compact",
    maximumFractionDigits: digits,
  }).format(value);
}

export function formatDuration(ms: number) {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

export function formatRemaining(endTs?: number | null) {
  if (!endTs) return "--";
  const remainingMs = endTs * 1000 - Date.now();
  if (remainingMs <= 0) return "Ended";
  return formatDuration(remainingMs);
}

export function formatTimestamp(timestamp: string) {
  if (!timestamp) return "--";
  if (timestamp.length >= 19 && timestamp.includes("T")) {
    return timestamp.slice(11, 19);
  }
  return timestamp;
}

export function truncateText(value: string, maxLength: number) {
  if (value.length <= maxLength) return value;
  return `${value.slice(0, Math.max(0, maxLength - 1))}…`;
}

export function walletLabel(walletAddress?: string | null, isLive?: boolean) {
  if (!walletAddress) return isLive ? "Unavailable" : "Paper ledger";
  return `${walletAddress.slice(0, 8)}...${walletAddress.slice(-6)}`;
}
