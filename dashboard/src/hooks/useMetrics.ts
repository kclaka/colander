import { useEffect, useState } from "react";
import type { MetricsSnapshot, PolicyMetrics } from "../types";

const MAX_HISTORY = 600;
const RECONNECT_DELAY = 2000;

function isPolicy(value: unknown): value is PolicyMetrics {
  if (!value || typeof value !== "object") return false;
  const policy = value as Record<string, unknown>;
  return typeof policy.name === "string" &&
    ["hit_rate", "hits", "misses", "evictions", "size", "capacity"].every(
      (key) => typeof policy[key] === "number" && Number.isFinite(policy[key]),
    );
}

function isSnapshot(value: unknown): value is MetricsSnapshot {
  if (!value || typeof value !== "object") return false;
  const snapshot = value as Record<string, unknown>;
  return isPolicy(snapshot.primary) &&
    (snapshot.comparison === null || isPolicy(snapshot.comparison)) &&
    (snapshot.mode === "demo" || snapshot.mode === "bench") &&
    ["timestamp_ms", "window_ms", "throughput_rps", "uptime_seconds"].every(
      (key) => typeof snapshot[key] === "number" && Number.isFinite(snapshot[key]),
    );
}

export function useMetrics() {
  const [history, setHistory] = useState<MetricsSnapshot[]>([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    // Each effect owns its socket and timer, including StrictMode's first mount.
    let disposed = false;
    let socket: WebSocket | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const connect = () => {
      if (disposed) return;
      const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(`${protocol}//${window.location.host}/ws/metrics`);
      socket = ws;
      ws.onopen = () => { if (!disposed && socket === ws) setConnected(true); };
      ws.onmessage = (event) => {
        if (disposed || socket !== ws) return;
        try {
          const snapshot: unknown = JSON.parse(event.data);
          if (!isSnapshot(snapshot)) return;
          setHistory((previous) => {
            const last = previous.at(-1);
            const changedPolicy = last && (last.primary.name !== snapshot.primary.name || last.comparison?.name !== snapshot.comparison?.name);
            const restarted = last && snapshot.uptime_seconds < last.uptime_seconds;
            return [...(changedPolicy || restarted ? [] : previous), snapshot].slice(-MAX_HISTORY);
          });
        } catch { /* Ignore malformed messages. */ }
      };
      ws.onclose = () => {
        if (disposed || socket !== ws) return;
        socket = null;
        setConnected(false);
        timer = setTimeout(connect, RECONNECT_DELAY);
      };
      ws.onerror = () => ws.close();
    };
    connect();
    return () => {
      disposed = true;
      clearTimeout(timer);
      if (socket) {
        socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
        socket.close();
      }
    };
  }, []);

  const latest = history.at(-1) ?? null;
  return { history, latest, connected };
}
