import type { MetricsSnapshot } from "../types";

interface Props {
  latest: MetricsSnapshot | null;
}

export function ModeToggle({ latest }: Props) {
  const currentMode = latest?.mode || "demo";

  const toggle = async () => {
    const newMode = currentMode === "demo" ? "bench" : "demo";
    try {
      await fetch("/api/mode", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ mode: newMode }),
      });
    } catch {
      // ignore
    }
  };

  return (
    <button
      onClick={toggle}
      className={`px-4 py-2 rounded-lg text-sm font-medium transition-colors ${
        currentMode === "demo"
          ? "bg-emerald-500/20 text-emerald-400 border border-emerald-500/30 hover:bg-emerald-500/30"
          : "bg-amber-500/20 text-amber-400 border border-amber-500/30 hover:bg-amber-500/30"
      }`}
    >
      {currentMode === "demo" ? "Demo Mode" : "Bench Mode"}
    </button>
  );
}
