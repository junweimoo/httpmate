import { useStore } from "../store";

export function StatusBar() {
  const status = useStore((s) => s.status);
  const order = useStore((s) => s.order);
  const filter = useStore((s) => s.filter);

  return (
    <div className="statusbar">
      <span className={status.running ? "dot on" : "dot off"} />
      <span>
        {status.running ? `listening on ${status.addr}` : "proxy stopped"}
        {status.systemProxyEnabled ? " · system proxy ON" : ""}
      </span>
      <span className="spacer" />
      <span>
        {order.length} transaction{order.length === 1 ? "" : "s"}
        {filter ? " (filtered)" : ""}
      </span>
    </div>
  );
}
