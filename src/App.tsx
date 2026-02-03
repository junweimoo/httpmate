import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { useStore } from "./store";
import { Toolbar } from "./components/Toolbar";
import { TrafficTable } from "./components/TrafficTable";
import { DetailPane } from "./components/DetailPane";
import { StatusBar } from "./components/StatusBar";
import { CaModal } from "./components/CaModal";
import type { CaState, ProxyEvent, ProxyStatus } from "./types";

export default function App() {
  const init = useStore((s) => s.init);
  const applyBatch = useStore((s) => s.applyBatch);
  const setStatus = useStore((s) => s.setStatus);
  const setCa = useStore((s) => s.setCa);
  const caModalOpen = useStore((s) => s.caModalOpen);
  const error = useStore((s) => s.error);
  const setError = useStore((s) => s.setError);

  useEffect(() => {
    const unlisteners = [
      listen<ProxyEvent[]>("traffic:batch", (e) => applyBatch(e.payload)),
      listen<ProxyStatus>("proxy:state", (e) => setStatus(e.payload)),
      listen<CaState>("ca:state", (e) => setCa(e.payload)),
    ];
    void init();
    return () => {
      for (const u of unlisteners) void u.then((f) => f());
    };
  }, [init, applyBatch, setStatus, setCa]);

  return (
    <div className="app">
      <Toolbar />
      {error && (
        <div className="error-banner">
          <span>{error}</span>
          <button onClick={() => setError(null)}>×</button>
        </div>
      )}
      <div className="main">
        <TrafficTable />
        <DetailPane />
      </div>
      <StatusBar />
      {caModalOpen && <CaModal />}
    </div>
  );
}
