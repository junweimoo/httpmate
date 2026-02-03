import type { TransactionDetail } from "./types";

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(2)} MB`;
}

export function fmtDuration(ms: number | null): string {
  if (ms === null) return "—";
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

export function fmtTime(epochMs: number): string {
  return new Date(epochMs).toLocaleTimeString(undefined, { hour12: false });
}

export function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function isTextual(contentType: string | null): boolean {
  if (!contentType) return false;
  const ct = contentType.toLowerCase();
  return (
    ct.startsWith("text/") ||
    ct.includes("json") ||
    ct.includes("xml") ||
    ct.includes("javascript") ||
    ct.includes("urlencoded") ||
    ct.includes("svg") ||
    ct.includes("graphql")
  );
}

export function hexDump(bytes: Uint8Array, maxBytes = 4096): string {
  const n = Math.min(bytes.length, maxBytes);
  const lines: string[] = [];
  for (let off = 0; off < n; off += 16) {
    const chunk = bytes.subarray(off, Math.min(off + 16, n));
    const hex = [...chunk]
      .map((b) => b.toString(16).padStart(2, "0"))
      .join(" ")
      .padEnd(47, " ");
    const ascii = [...chunk]
      .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : "."))
      .join("");
    lines.push(`${off.toString(16).padStart(8, "0")}  ${hex}  ${ascii}`);
  }
  if (bytes.length > maxBytes) {
    lines.push(`… ${fmtBytes(bytes.length - maxBytes)} more`);
  }
  return lines.join("\n");
}

export function fullUrl(d: TransactionDetail): string {
  const s = d.summary;
  return `${s.scheme}://${s.host}${s.path}${s.query ? `?${s.query}` : ""}`;
}

/** Build a copy-pasteable cURL command from the recorded request. */
export function toCurl(d: TransactionDetail): string {
  const parts = [`curl -X ${d.summary.method} '${fullUrl(d)}'`];
  for (const [name, value] of d.reqHeaders) {
    const lower = name.toLowerCase();
    if (lower === "content-length" || lower === "host") continue;
    parts.push(`  -H '${name}: ${value.replace(/'/g, "'\\''")}'`);
  }
  if (d.reqBodyTotal > 0 && !d.reqBodyTruncated) {
    try {
      const text = new TextDecoder("utf-8", { fatal: true }).decode(
        b64ToBytes(d.reqBodyBase64),
      );
      parts.push(`  --data-raw '${text.replace(/'/g, "'\\''")}'`);
    } catch {
      parts.push("  # binary request body omitted");
    }
  } else if (d.reqBodyTruncated) {
    parts.push("  # request body truncated in capture; omitted");
  }
  return parts.join(" \\\n");
}
