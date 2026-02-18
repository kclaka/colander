import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from "recharts";
import type { MetricsSnapshot } from "../types";

interface Props {
  history: MetricsSnapshot[];
}

export function ThroughputChart({ history }: Props) {
  const data = history.map((s, i) => ({
    idx: i,
    time: Math.floor(s.uptime_seconds),
    rps: Math.round(s.throughput_rps),
  }));

  return (
    <div className="rounded-xl bg-slate-800/50 p-4 border border-slate-700/50">
      <h2 className="text-sm font-medium text-slate-400 mb-3">
        Throughput (req/s)
      </h2>
      <ResponsiveContainer width="100%" height={200}>
        <AreaChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#334155" />
          <XAxis dataKey="time" stroke="#64748b" tick={{ fontSize: 11 }} />
          <YAxis stroke="#64748b" tick={{ fontSize: 11 }} />
          <Tooltip
            contentStyle={{
              background: "#1e293b",
              border: "1px solid #334155",
              borderRadius: 8,
              fontSize: 12,
            }}
          />
          <Area
            type="monotone"
            dataKey="rps"
            stroke="#3b82f6"
            fill="#3b82f6"
            fillOpacity={0.15}
            strokeWidth={2}
            isAnimationActive={false}
          />
        </AreaChart>
      </ResponsiveContainer>
    </div>
  );
}
