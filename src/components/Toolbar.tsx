import { useEffect, useState } from "react";
import { useStore } from "../store";

export function Toolbar() {
  const status = useStore((s) => s.status);
  const settings = useStore((s) => s.settings);
  const filter = useStore((s) => s.filter);
  const setFilter = useStore((s) => s.setFilter);
  const startProxy = useStore((s) => s.startProxy);
  const stopProxy = useStore((s) => s.stopProxy);
  const toggleSystemProxy = useStore((s) => s.toggleSystemProxy);
  const clear = useStore((s) => s.clear);
  const setCaModalOpen = useStore((s) => s.setCaModalOpen);
  const ca = useStore((s) => s.ca);

  const [port, setPort] = useState<string>("");
  useEffect(() => {
    if (settings && port === "") setPort(String(settings.port));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings]);

  const onStartStop = () => {
    if (status.running) {
      void stopProxy();
    } else {
      const parsed = parseInt(port, 10);
      void startProxy(Number.isFinite(parsed) && parsed > 0 ? parsed : undefined);
    }
  };

  const caBadge =
    !ca || !ca.generated ? "!" : ca.trusted === false ? "!" : null;

  return (
    <div className="toolbar">
      <span className="brand">httpmate</span>
      <button
        className={status.running ? "btn stop" : "btn start"}
        onClick={onStartStop}
      >
        {status.running ? "■ Stop" : "▶ Start"}
      </button>
      <label className="port-label">
        port
        <input
          className="port-input"
          value={port}
          disabled={status.running}
          onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
          inputMode="numeric"
        />
      </label>
      <label className="sysproxy" title="Route this Mac's HTTP(S) traffic through httpmate">
        <input
          type="checkbox"
          checked={status.systemProxyEnabled}
          disabled={!status.running && !status.systemProxyEnabled}
          onChange={() => void toggleSystemProxy()}
        />
        system proxy
      </label>
      <input
        className="filter-input"
        placeholder="filter host, path, method, status…"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <button className="btn" onClick={() => void clear()} title="Clear captured session">
        Clear
      </button>
      <button className="btn" onClick={() => setCaModalOpen(true)}>
        CA{caBadge && <span className="badge">{caBadge}</span>}
      </button>
    </div>
  );
}
