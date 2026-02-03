// Mirrors the serde-camelCase DTOs in crates/httpmate-core/src/events.rs.

export type TxState = "active" | "completed" | "failed";

export interface TransactionSummary {
  id: number;
  startedAtMs: number;
  kind: "http" | "tunnel" | "ws-upgrade" | string;
  scheme: string;
  method: string;
  host: string;
  path: string;
  query: string | null;
  status: number | null;
  durationMs: number | null;
  reqSize: number;
  respSize: number;
  contentType: string | null;
  error: string | null;
  state: TxState;
}

export interface TransactionDetail {
  summary: TransactionSummary;
  httpVersion: string;
  clientAddr: string;
  tlsVersion: string | null;
  alpn: string | null;
  reqHeaders: [string, string][];
  respHeaders: [string, string][];
  reqBodyBase64: string;
  reqBodyTruncated: boolean;
  reqBodyTotal: number;
  respBodyBase64: string;
  respBodyTruncated: boolean;
  respBodyTotal: number;
  tags: unknown;
}

export interface ProxyStatus {
  running: boolean;
  addr: string | null;
  port: number | null;
  systemProxyEnabled: boolean;
}

export interface CaState {
  generated: boolean;
  certPath: string | null;
  trusted: boolean | null;
}

export interface ProxySettings {
  port: number;
  bindAddr: string;
  bodyCaptureLimit: number;
  passthroughHosts: string[];
  extraRootCertsPem: string[];
}

export interface QueryFilter {
  search?: string | null;
  host?: string | null;
  method?: string | null;
  status?: number | null;
  beforeId?: number | null;
  limit?: number | null;
}

export type ProxyEvent =
  | { type: "transactionStarted"; data: TransactionSummary }
  | { type: "transactionUpdated"; data: TransactionSummary }
  | { type: "transactionCompleted"; data: TransactionSummary }
  | { type: "proxyState"; data: ProxyStatus }
  | { type: "caState"; data: CaState };

export interface ExportedCa {
  pem: string;
  path: string;
}
