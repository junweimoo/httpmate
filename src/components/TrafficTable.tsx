import { useEffect, useMemo, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { filteredOrder, useStore } from "../store";
import { fmtBytes, fmtDuration, fmtTime } from "../format";

const ROW_HEIGHT = 26;

function statusClass(status: number | null, state: string): string {
  if (state === "failed") return "st-error";
  if (status === null) return "st-pending";
  if (status < 300) return "st-2xx";
  if (status < 400) return "st-3xx";
  if (status < 500) return "st-4xx";
  return "st-5xx";
}

export function TrafficTable() {
  const txs = useStore((s) => s.txs);
  const order = useStore((s) => s.order);
  const filter = useStore((s) => s.filter);
  const selectedId = useStore((s) => s.selectedId);
  const select = useStore((s) => s.select);

  const visible = useMemo(() => filteredOrder(order, txs, filter), [order, txs, filter]);

  const parentRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  const virtualizer = useVirtualizer({
    count: visible.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 20,
  });

  // Follow live traffic unless the user scrolled up.
  useEffect(() => {
    const el = parentRef.current;
    if (!el) return;
    const onScroll = () => {
      stickToBottom.current =
        el.scrollHeight - el.scrollTop - el.clientHeight < ROW_HEIGHT * 3;
    };
    el.addEventListener("scroll", onScroll);
    return () => el.removeEventListener("scroll", onScroll);
  }, []);

  useEffect(() => {
    if (stickToBottom.current && visible.length > 0) {
      virtualizer.scrollToIndex(visible.length - 1, { align: "end" });
    }
  }, [visible.length, virtualizer]);

  return (
    <div className="traffic-pane">
      <div className="table-header">
        <span className="col-id">#</span>
        <span className="col-time">time</span>
        <span className="col-method">method</span>
        <span className="col-status">status</span>
        <span className="col-host">host</span>
        <span className="col-path">path</span>
        <span className="col-dur">time</span>
        <span className="col-size">size</span>
      </div>
      <div className="table-body" ref={parentRef}>
        {visible.length === 0 ? (
          <div className="empty-state">
            {order.length === 0
              ? "No traffic yet. Start the proxy and point a client at it."
              : "Nothing matches the filter."}
          </div>
        ) : (
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((row) => {
              const s = txs.get(visible[row.index]);
              if (!s) return null;
              return (
                <div
                  key={s.id}
                  className={
                    "table-row" +
                    (s.id === selectedId ? " selected" : "") +
                    (s.state === "failed" ? " failed" : "") +
                    (s.state === "active" ? " active" : "")
                  }
                  style={{
                    position: "absolute",
                    top: 0,
                    transform: `translateY(${row.start}px)`,
                    height: ROW_HEIGHT,
                  }}
                  onClick={() => void select(s.id)}
                >
                  <span className="col-id">{s.id}</span>
                  <span className="col-time">{fmtTime(s.startedAtMs)}</span>
                  <span className={`col-method m-${s.method.toLowerCase()}`}>
                    {s.kind === "tunnel" ? "TUNNEL" : s.method}
                  </span>
                  <span className={`col-status ${statusClass(s.status, s.state)}`}>
                    {s.state === "failed" ? "✕" : (s.status ?? "…")}
                  </span>
                  <span className="col-host" title={s.host}>
                    {s.scheme === "https" ? "🔒 " : ""}
                    {s.host}
                  </span>
                  <span className="col-path" title={s.path + (s.query ? `?${s.query}` : "")}>
                    {s.kind === "tunnel" ? "(opaque tunnel)" : s.path}
                  </span>
                  <span className="col-dur">{fmtDuration(s.durationMs)}</span>
                  <span className="col-size">{fmtBytes(s.respSize)}</span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
