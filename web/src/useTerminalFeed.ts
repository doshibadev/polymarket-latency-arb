import { startTransition, useEffect, useRef, useState } from "react";
import { getDashboardAuthToken } from "./api";
import { EMPTY_SNAPSHOT, FeedStatus, SnapshotMessage, TerminalSnapshot } from "./types";

export function useTerminalFeed() {
  const [snapshot, setSnapshot] = useState<TerminalSnapshot>(EMPTY_SNAPSHOT);
  const [status, setStatus] = useState<FeedStatus>("connecting");
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null);
  const snapshotRef = useRef<TerminalSnapshot>(EMPTY_SNAPSHOT);
  const pendingSnapshotRef = useRef<TerminalSnapshot | null>(null);
  const frameRef = useRef<number | null>(null);

  useEffect(() => {
    let active = true;
    let reconnectTimers: number[] = [];
    const sockets = new Map<string, WebSocket>();
    let openSockets = 0;

    const scheduleSnapshotApply = () => {
      if (frameRef.current !== null) return;
      frameRef.current = window.requestAnimationFrame(() => {
        frameRef.current = null;
        const next = pendingSnapshotRef.current;
        if (!active || !next) return;
        pendingSnapshotRef.current = null;
        snapshotRef.current = next;
        startTransition(() => {
          setSnapshot(next);
          setLastUpdatedAt(Date.now());
        });
      });
    };

    const mergeSnapshot = (incoming: TerminalSnapshot | SnapshotMessage) => {
      const base = pendingSnapshotRef.current ?? snapshotRef.current;
      if (
        typeof incoming === "object" &&
        incoming !== null &&
        "_kind" in incoming &&
        (incoming._kind === "fast" || incoming._kind === "slow")
      ) {
        pendingSnapshotRef.current = { ...base, ...incoming.snapshot };
      } else {
        pendingSnapshotRef.current = incoming as TerminalSnapshot;
      }
      scheduleSnapshotApply();
    };

    const connect = (path: "/ws" | "/ws/fast" | "/ws/slow") => {
      if (!active) return;
      const protocol = window.location.protocol === "https:" ? "wss" : "ws";
      const url = new URL(`${protocol}://${window.location.host}${path}`);
      const token = getDashboardAuthToken();
      if (token) {
        url.searchParams.set("token", token);
      }
      const socket = new WebSocket(url);
      sockets.set(path, socket);
      startTransition(() => setStatus("connecting"));

      socket.onopen = () => {
        if (!active) return;
        openSockets += 1;
        startTransition(() => setStatus("live"));
      };

      socket.onmessage = (event) => {
        if (!active) return;
        try {
          mergeSnapshot(JSON.parse(event.data) as TerminalSnapshot | SnapshotMessage);
        } catch {
          startTransition(() => setStatus("offline"));
        }
      };

      socket.onerror = () => {
        if (!active) return;
        startTransition(() => setStatus("offline"));
      };

      socket.onclose = () => {
        if (!active) return;
        sockets.delete(path);
        openSockets = Math.max(0, openSockets - 1);
        if (openSockets === 0) {
          startTransition(() => setStatus("offline"));
        }
        reconnectTimers.push(window.setTimeout(() => connect(path), 1500));
      };
    };

    connect("/ws/fast");
    connect("/ws/slow");

    return () => {
      active = false;
      for (const timer of reconnectTimers) window.clearTimeout(timer);
      if (frameRef.current !== null) {
        window.cancelAnimationFrame(frameRef.current);
      }
      for (const socket of sockets.values()) socket.close();
    };
  }, []);

  return {
    snapshot,
    status,
    lastUpdatedAt,
  };
}
