import { useMetrics } from "./hooks/useMetrics";
import { HitRateChart } from "./components/HitRateChart";
import { ThroughputChart } from "./components/ThroughputChart";
import { StatsCards } from "./components/StatsCards";
import { AlphaSlider } from "./components/AlphaSlider";
import { ModeToggle } from "./components/ModeToggle";
import { ConnectionStatus } from "./components/ConnectionStatus";

function App() {
  const { history, latest, connected } = useMetrics();

  return (
    <div className="min-h-screen bg-slate-900 p-4 md:p-6">
      <div className="max-w-6xl mx-auto space-y-4">
        {/* Header */}
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-xl font-bold text-white tracking-tight">
              colander
            </h1>
            <p className="text-xs text-slate-500">
              SIEVE vs LRU — Live Cache Performance
            </p>
          </div>
          <div className="flex items-center gap-3">
            <ConnectionStatus
              connected={connected}
              uptime={latest?.uptime_seconds ?? null}
            />
            <ModeToggle latest={latest} />
          </div>
        </div>

        {/* Stats */}
        <StatsCards latest={latest} />

        {/* Charts */}
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
          <HitRateChart history={history} />
          <ThroughputChart history={history} />
        </div>

        {/* Controls */}
        <AlphaSlider />
      </div>
    </div>
  );
}

export default App;
