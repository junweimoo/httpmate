import { invoke } from "@tauri-apps/api/core";
import type {
  CaState,
  ExportedCa,
  ProxySettings,
  ProxyStatus,
  QueryFilter,
  TransactionDetail,
  TransactionSummary,
} from "./types";

export const api = {
  startProxy: (port?: number) =>
    invoke<ProxyStatus>("start_proxy", { port: port ?? null }),
  stopProxy: () => invoke<ProxyStatus>("stop_proxy"),
  getStatus: () => invoke<ProxyStatus>("get_status"),
  setSystemProxy: (enabled: boolean) =>
    invoke<ProxyStatus>("set_system_proxy", { enabled }),
  queryTransactions: (filter: QueryFilter) =>
    invoke<TransactionSummary[]>("query_transactions", { filter }),
  getTransaction: (id: number) =>
    invoke<TransactionDetail | null>("get_transaction", { id }),
  clearSession: () => invoke<void>("clear_session"),
  getSettings: () => invoke<ProxySettings>("get_settings"),
  setSettings: (settings: ProxySettings) =>
    invoke<void>("set_settings", { settings }),
  caState: () => invoke<CaState>("ca_state"),
  exportCa: () => invoke<ExportedCa>("export_ca"),
  installCaTrust: () => invoke<CaState>("install_ca_trust"),
};
