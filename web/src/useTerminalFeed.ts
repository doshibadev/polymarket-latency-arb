import { useEffect, useState } from "react";
import { EMPTY_SNAPSHOT, FeedStatus, TerminalSnapshot } from "./types";

export function useTerminalFeed() {
  const [snapshot, setSnapshot] = useState<TerminalSnapshot>(EMPTY_SNAPSHOT);
  const [status, setStatus] = useState<FeedStatus>("connecting");
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null);

  useEffect(() => {
    let active = true;
    let reconnectTimer: number | null = null;
    let socket: WebSocket | null = null;

    const connect = () => {
      if (!active) return;
      const protocol = window.location.protocol === "https:" ? "wss" : "ws";
      socket = new WebSocket(`${protocol}://${window.location.host}/ws`);
      setStatus("connecting");

      socket.onopen = () => {
        if (!active) return;
        setStatus("live");
      };

      socket.onmessage = (event) => {
        if (!active) return;
        try {
          const next = JSON.parse(event.data) as TerminalSnapshot;
          setSnapshot(next);
          setLastUpdatedAt(Date.now());
        } catch {
          setStatus("offline");
        }
      };

      socket.onerror = () => {
        if (!active) return;
        setStatus("offline");
      };

      socket.onclose = () => {
        if (!active) return;
        setStatus("offline");
        reconnectTimer = window.setTimeout(connect, 1500);
      };
    };

    connect();

    return () => {
      active = false;
      if (reconnectTimer !== null) window.clearTimeout(reconnectTimer);
      socket?.close();
    };
  }, []);

  return {
    snapshot,
    status,
    lastUpdatedAt,
  };
}
