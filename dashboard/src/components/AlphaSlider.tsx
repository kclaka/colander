import { useState, useEffect, useCallback } from "react";

interface LoadGenStatus {
  alpha: number;
  running: boolean;
  total_requests: number;
}

export function AlphaSlider() {
  const [alpha, setAlpha] = useState(0.8);
  const [connected, setConnected] = useState(false);
  const [totalRequests, setTotalRequests] = useState(0);

  // Poll loadgen status
  useEffect(() => {
    const poll = async () => {
      try {
        const res = await fetch("/status");
        if (res.ok) {
          const data: LoadGenStatus = await res.json();
          setAlpha(data.alpha);
          setTotalRequests(data.total_requests);
          setConnected(true);
        } else {
          setConnected(false);
        }
      } catch {
        setConnected(false);
      }
    };
    poll();
    const interval = setInterval(poll, 2000);
    return () => clearInterval(interval);
  }, []);

  const sendAlpha = useCallback(async (newAlpha: number) => {
    try {
      await fetch("/control", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ alpha: newAlpha }),
      });
    } catch {
      // ignore
    }
  }, []);

  const handleChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const val = parseFloat(e.target.value);
    setAlpha(val);
    sendAlpha(val);
  };

  return (
    <div className="rounded-xl bg-slate-800/50 p-4 border border-slate-700/50">
      <div className="flex items-center justify-between mb-2">
        <h2 className="text-sm font-medium text-slate-400">
          Zipfian Alpha ({"\u03B1"})
        </h2>
        {connected ? (
          <span className="text-xs text-emerald-400">
            {totalRequests.toLocaleString()} reqs
          </span>
        ) : (
          <span className="text-xs text-amber-400">loadgen offline</span>
        )}
      </div>
      <div className="flex items-center gap-4">
        <input
          type="range"
          min="0.1"
          max="2.0"
          step="0.05"
          value={alpha}
          onChange={handleChange}
          className="flex-1 h-2 rounded-lg appearance-none cursor-pointer accent-blue-500 bg-slate-600"
        />
        <span className="text-lg font-mono font-bold text-blue-400 w-12 text-right">
          {alpha.toFixed(2)}
        </span>
      </div>
      <div className="flex justify-between text-xs text-slate-500 mt-1 px-1">
        <span>0.1 (uniform)</span>
        <span>2.0 (very skewed)</span>
      </div>
      <p className="text-xs text-slate-500 mt-2">
        Higher {"\u03B1"} = more skewed traffic = SIEVE pulls further ahead of
        LRU
      </p>
    </div>
  );
}
