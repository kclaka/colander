import { useEffect, useRef, useState, useCallback } from "react";
import type { MetricsSnapshot } from "../types";

const MAX_HISTORY = 600; // 5 minutes at 500ms intervals
const RECONNECT_DELAY = 2000;

export function useMetrics() {
  const [history, setHistory] = useState<MetricsSnapshot[]>([]);
  const [connected, setConnected] = useState(false);
  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const connect = useCallback(() => {
    if (wsRef.current?.readyState === WebSocket.OPEN) return;

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${protocol}//${window.location.host}/ws/metrics`);
    wsRef.current = ws;

    ws.onopen = () => setConnected(true);

    ws.onmessage = (event) => {
      try {
        const snapshot: MetricsSnapshot = JSON.parse(event.data);
        setHistory((prev) => {
          const next = [...prev, snapshot];
          return next.length > MAX_HISTORY ? next.slice(-MAX_HISTORY) : next;
        });
      } catch {
        // ignore parse errors
      }
    };

    ws.onclose = () => {
      setConnected(false);
      reconnectTimer.current = setTimeout(connect, RECONNECT_DELAY);
    };

    ws.onerror = () => ws.close();
  }, []);

  useEffect(() => {
    connect();
    return () => {
      wsRef.current?.close();
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
    };
  }, [connect]);

  const latest = history.length > 0 ? history[history.length - 1] : null;

  return { history, latest, connected };
}
