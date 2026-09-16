import { useState } from "react";
import type { MetricsSnapshot } from "../types";

interface Props {
  latest: MetricsSnapshot | null;
}

export function ModeToggle({ latest }: Props) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const currentMode = latest?.mode || "demo";

  const toggle = async () => {
    if (pending) return;
    setPending(true);
    setError(null);
    const newMode = currentMode === "demo" ? "bench" : "demo";
    try {
      const response = await fetch("/api/mode", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ mode: newMode }),
      });
      if (!response.ok) throw new Error("Mode change failed");
    } catch {
      setError("Could not change mode. Try again.");
    } finally {
      setPending(false);
    }
  };

  return (
    <div>
    <button
      disabled={pending || !latest}
      aria-busy={pending}
      onClick={toggle}
      className={`px-4 py-2 rounded-lg text-sm font-medium transition-colors ${
        currentMode === "demo"
          ? "bg-emerald-500/20 text-emerald-400 border border-emerald-500/30 hover:bg-emerald-500/30"
          : "bg-amber-500/20 text-amber-400 border border-amber-500/30 hover:bg-amber-500/30"
      }`}
    >
      {currentMode === "demo" ? "Demo Mode" : "Bench Mode"}
    </button>
    {error && <p role="alert" className="text-sm text-red-400 mt-1">{error}</p>}
    </div>
  );
}
