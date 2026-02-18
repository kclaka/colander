export interface PolicyMetrics {
  name: string;
  hit_rate: number;
  hits: number;
  misses: number;
  evictions: number;
  size: number;
  capacity: number;
}

export interface MetricsSnapshot {
  timestamp_ms: number;
  window_ms: number;
  primary: PolicyMetrics;
  comparison: PolicyMetrics | null;
  throughput_rps: number;
  uptime_seconds: number;
  mode: string;
}
