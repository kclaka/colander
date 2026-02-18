interface Props {
  connected: boolean;
  uptime: number | null;
}

export function ConnectionStatus({ connected, uptime }: Props) {
  const formatUptime = (s: number) => {
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${sec}s`;
    return `${sec}s`;
  };

  return (
    <div className="flex items-center gap-2 text-xs">
      <span
        className={`h-2 w-2 rounded-full ${connected ? "bg-emerald-400" : "bg-red-400 animate-pulse"}`}
      />
      <span className={connected ? "text-slate-400" : "text-red-400"}>
        {connected
          ? `Connected${uptime != null ? ` · ${formatUptime(uptime)}` : ""}`
          : "Disconnected"}
      </span>
    </div>
  );
}
