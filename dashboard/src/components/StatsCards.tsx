import type { MetricsSnapshot } from "../types";

interface Props {
  latest: MetricsSnapshot | null;
}

function StatCard({
  label,
  value,
  sub,
  color,
}: {
  label: string;
  value: string;
  sub?: string;
  color?: string;
}) {
  return (
    <div className="rounded-xl bg-slate-800/50 p-4 border border-slate-700/50">
      <p className="text-xs font-medium text-slate-400 uppercase tracking-wider">
        {label}
      </p>
      <p className={`text-2xl font-bold mt-1 ${color || "text-white"}`}>
        {value}
      </p>
      {sub && <p className="text-xs text-slate-500 mt-1">{sub}</p>}
    </div>
  );
}

export function StatsCards({ latest }: Props) {
  if (!latest) {
    return (
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        {[...Array(4)].map((_, i) => (
          <div
            key={i}
            className="rounded-xl bg-slate-800/50 p-4 border border-slate-700/50 animate-pulse h-24"
          />
        ))}
      </div>
    );
  }

  const p = latest.primary;
  const c = latest.comparison;

  const sieveHR = (p.hit_rate * 100).toFixed(1);
  const lruHR = c ? (c.hit_rate * 100).toFixed(1) : "—";
  const advantage =
    c && c.hit_rate > 0
      ? ((p.hit_rate - c.hit_rate) / c.hit_rate * 100).toFixed(1)
      : null;

  return (
    <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
      <StatCard
        label="SIEVE Hit Rate"
        value={`${sieveHR}%`}
        sub={`${p.hits.toLocaleString()} hits / ${p.misses.toLocaleString()} misses`}
        color="text-cyan-400"
      />
      <StatCard
        label="LRU Hit Rate"
        value={`${lruHR}%`}
        sub={
          c
            ? `${c.hits.toLocaleString()} hits / ${c.misses.toLocaleString()} misses`
            : "no comparison"
        }
        color="text-pink-400"
      />
      <StatCard
        label="SIEVE Advantage"
        value={advantage ? `+${advantage}%` : "—"}
        sub="relative to LRU"
        color={
          advantage && parseFloat(advantage) > 0
            ? "text-emerald-400"
            : "text-slate-400"
        }
      />
      <StatCard
        label="Throughput"
        value={`${Math.round(latest.throughput_rps)} rps`}
        sub={`${p.size.toLocaleString()} / ${p.capacity.toLocaleString()} cached`}
      />
    </div>
  );
}
