import { useMemo, useState } from "react";
import { useStore } from "../store";
import {
  b64ToBytes,
  fmtBytes,
  fmtDuration,
  fullUrl,
  hexDump,
  isTextual,
  toCurl,
} from "../format";
import type { TransactionDetail } from "../types";

type Tab = "request" | "response";

function headerValue(headers: [string, string][], name: string): string | null {
  const found = headers.find(([n]) => n.toLowerCase() === name.toLowerCase());
  return found ? found[1] : null;
}

function BodyView({
  bodyB64,
  contentType,
  truncated,
  total,
}: {
  bodyB64: string;
  contentType: string | null;
  truncated: boolean;
  total: number;
}) {
  const bytes = useMemo(() => b64ToBytes(bodyB64), [bodyB64]);

  if (bytes.length === 0) {
    return <div className="body-empty">{total > 0 ? `(${fmtBytes(total)} not captured)` : "(no body)"}</div>;
  }

  const ct = contentType?.toLowerCase() ?? null;

  if (ct?.startsWith("image/") && !ct.includes("svg")) {
    return (
      <div className="body-image">
        <img src={`data:${ct};base64,${bodyB64}`} alt="response body" />
      </div>
    );
  }

  let content: string;
  let cls = "body-text";
  if (isTextual(ct)) {
    try {
      const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      if (ct?.includes("json")) {
        try {
          content = JSON.stringify(JSON.parse(text), null, 2);
        } catch {
          content = text;
        }
      } else {
        content = text;
      }
    } catch {
      content = hexDump(bytes);
      cls = "body-hex";
    }
  } else {
    // Unknown/binary: try text, fall back to hex.
    try {
      content = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch {
      content = hexDump(bytes);
      cls = "body-hex";
    }
  }

  return (
    <div className="body-wrap">
      {truncated && (
        <div className="truncation-note">
          capture truncated — showing {fmtBytes(bytes.length)} of {fmtBytes(total)}
        </div>
      )}
      <pre className={cls}>{content}</pre>
    </div>
  );
}

function HeadersView({ headers }: { headers: [string, string][] }) {
  if (headers.length === 0) return <div className="body-empty">(no headers)</div>;
  return (
    <table className="headers-table">
      <tbody>
        {headers.map(([name, value], i) => (
          <tr key={i}>
            <td className="hdr-name">{name}</td>
            <td className="hdr-value">{value}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Side({ d, tab }: { d: TransactionDetail; tab: Tab }) {
  const headers = tab === "request" ? d.reqHeaders : d.respHeaders;
  const bodyB64 = tab === "request" ? d.reqBodyBase64 : d.respBodyBase64;
  const truncated = tab === "request" ? d.reqBodyTruncated : d.respBodyTruncated;
  const total = tab === "request" ? d.reqBodyTotal : d.respBodyTotal;
  const contentType = headerValue(headers, "content-type");
  return (
    <>
      <div className="section-label">Headers</div>
      <HeadersView headers={headers} />
      <div className="section-label">Body</div>
      <BodyView bodyB64={bodyB64} contentType={contentType} truncated={truncated} total={total} />
    </>
  );
}

export function DetailPane() {
  const detail = useStore((s) => s.detail);
  const detailLoading = useStore((s) => s.detailLoading);
  const selectedId = useStore((s) => s.selectedId);
  const [tab, setTab] = useState<Tab>("response");
  const [copied, setCopied] = useState(false);

  if (selectedId === null) {
    return (
      <div className="detail-pane">
        <div className="empty-state">Select a transaction to inspect it.</div>
      </div>
    );
  }
  if (detailLoading && !detail) {
    return (
      <div className="detail-pane">
        <div className="empty-state">Loading…</div>
      </div>
    );
  }
  if (!detail) {
    return (
      <div className="detail-pane">
        <div className="empty-state">
          Still in flight — details appear when the transaction completes.
        </div>
      </div>
    );
  }

  const s = detail.summary;
  const copyCurl = () => {
    void navigator.clipboard.writeText(toCurl(detail)).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  return (
    <div className="detail-pane">
      <div className="detail-header">
        <div className="detail-url" title={fullUrl(detail)}>
          <span className={`m-${s.method.toLowerCase()} method-chip`}>{s.method}</span>{" "}
          {fullUrl(detail)}
        </div>
        <div className="detail-meta">
          <span>{s.status ?? "—"}</span>
          <span>{fmtDuration(s.durationMs)}</span>
          <span>↑ {fmtBytes(s.reqSize)}</span>
          <span>↓ {fmtBytes(s.respSize)}</span>
          <span>{detail.httpVersion}</span>
          {detail.tlsVersion && <span>{detail.tlsVersion}</span>}
          {detail.alpn && <span>alpn:{detail.alpn}</span>}
          <span title="client address">{detail.clientAddr}</span>
        </div>
        {s.error && <div className="detail-error">{s.error}</div>}
      </div>
      <div className="detail-tabs">
        <button className={tab === "request" ? "tab on" : "tab"} onClick={() => setTab("request")}>
          Request
        </button>
        <button className={tab === "response" ? "tab on" : "tab"} onClick={() => setTab("response")}>
          Response
        </button>
        <span className="spacer" />
        <button className="btn small" onClick={copyCurl}>
          {copied ? "Copied!" : "Copy as cURL"}
        </button>
      </div>
      <div className="detail-body">
        <Side d={detail} tab={tab} />
      </div>
    </div>
  );
}
