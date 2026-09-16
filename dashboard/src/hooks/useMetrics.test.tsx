import { StrictMode } from "react";
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useMetrics } from "./useMetrics";

class FakeSocket {
  static instances: FakeSocket[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  close = vi.fn();
  constructor() { FakeSocket.instances.push(this); }
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeSocket.instances = [];
  vi.stubGlobal("WebSocket", FakeSocket);
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); });

describe("metrics socket lifecycle", () => {
  it("does not reconnect after cleanup, even for a queued close callback", () => {
    const { unmount } = renderHook(() => useMetrics());
    const socket = FakeSocket.instances[0];
    const queuedClose = socket.onclose;
    unmount();
    act(() => { queuedClose?.(); vi.advanceTimersByTime(10000); });
    expect(socket.close).toHaveBeenCalledOnce();
    expect(FakeSocket.instances).toHaveLength(1);
  });

  it("keeps only one live connection after StrictMode remount", () => {
    const { result } = renderHook(() => useMetrics(), { wrapper: StrictMode });
    expect(FakeSocket.instances).toHaveLength(2);
    expect(FakeSocket.instances[0].close).toHaveBeenCalledOnce();
    act(() => { FakeSocket.instances[1].onopen?.(); vi.advanceTimersByTime(10000); });
    expect(result.current.connected).toBe(true);
    expect(FakeSocket.instances).toHaveLength(2);
  });

  it("reconnects once after an unexpected close and cancels pending reconnects", () => {
    const { unmount } = renderHook(() => useMetrics());
    act(() => FakeSocket.instances[0].onclose?.());
    act(() => vi.advanceTimersByTime(2000));
    expect(FakeSocket.instances).toHaveLength(2);
    act(() => FakeSocket.instances[1].onclose?.());
    unmount();
    act(() => vi.advanceTimersByTime(5000));
    expect(FakeSocket.instances).toHaveLength(2);
  });

  it("ignores malformed payloads and retains a bounded valid history", () => {
    const { result } = renderHook(() => useMetrics());
    const socket = FakeSocket.instances[0];
    act(() => { for (const data of ["{", "null", "{}", '{"primary":{}}']) socket.onmessage?.({ data }); });
    expect(result.current.latest).toBeNull();
    const policy = { name: "SIEVE", hits: 1, misses: 0, hit_rate: 1, evictions: 0, size: 1, capacity: 64 };
    act(() => {
      for (let i = 0; i < 605; i++) socket.onmessage?.({ data: JSON.stringify({ timestamp_ms: i, window_ms: 500, primary: policy, comparison: null, throughput_rps: 2, uptime_seconds: 1, mode: "demo" }) });
    });
    expect(result.current.history).toHaveLength(600);
    expect(result.current.history[0].timestamp_ms).toBe(5);
    expect(result.current.latest?.timestamp_ms).toBe(604);
  });
});
