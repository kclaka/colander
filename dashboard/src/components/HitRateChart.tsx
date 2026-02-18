import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
} from "recharts";
import type { MetricsSnapshot } from "../types";

interface Props {
  history: MetricsSnapshot[];
}

export function HitRateChart({ history }: Props) {
  const data = history.map((s, i) => ({
    idx: i,
    time: Math.floor(s.uptime_seconds),
    sieve: +(s.primary.hit_rate * 100).toFixed(2),
    lru: s.comparison ? +(s.comparison.hit_rate * 100).toFixed(2) : null,
  }));

  return (
    <div className="rounded-xl bg-slate-800/50 p-4 border border-slate-700/50">
      <h2 className="text-sm font-medium text-slate-400 mb-3">
        Hit Rate (%)
      </h2>
      <ResponsiveContainer width="100%" height={300}>
        <LineChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#334155" />
          <XAxis
            dataKey="time"
            stroke="#64748b"
            tick={{ fontSize: 11 }}
            label={{
              value: "uptime (s)",
              position: "insideBottom",
              offset: -2,
              style: { fill: "#64748b", fontSize: 11 },
            }}
          />
          <YAxis
            domain={[0, 100]}
            stroke="#64748b"
            tick={{ fontSize: 11 }}
            tickFormatter={(v: number) => `${v}%`}
          />
          <Tooltip
            contentStyle={{
              background: "#1e293b",
              border: "1px solid #334155",
              borderRadius: 8,
              fontSize: 12,
            }}
            formatter={(value: number | undefined) =>
              value != null ? `${value.toFixed(2)}%` : ""
            }
          />
          <Legend />
          <Line
            type="monotone"
            dataKey="sieve"
            name="SIEVE"
            stroke="#22d3ee"
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
          />
          <Line
            type="monotone"
            dataKey="lru"
            name="LRU"
            stroke="#f472b6"
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
          />
        </LineChart>
      </ResponsiveContainer>
    </div>
  );
}
