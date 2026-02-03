import { create } from "zustand";
import { api } from "./ipc";
import type {
  CaState,
  ProxyEvent,
  ProxySettings,
  ProxyStatus,
  TransactionDetail,
  TransactionSummary,
} from "./types";

/** In-memory window for the live table; older rows stay queryable in SQLite. */
const MAX_ROWS = 5000;

interface AppStore {
  txs: Map<number, TransactionSummary>;
  order: number[]; // ascending ids
  status: ProxyStatus;
  settings: ProxySettings | null;
  ca: CaState | null;
  filter: string;
  selectedId: number | null;
  detail: TransactionDetail | null;
  detailLoading: boolean;
  caModalOpen: boolean;
  error: string | null;

  init(): Promise<void>;
  applyBatch(events: ProxyEvent[]): void;
  setStatus(status: ProxyStatus): void;
  setCa(ca: CaState): void;
  setFilter(filter: string): void;
  select(id: number | null): Promise<void>;
  startProxy(port?: number): Promise<void>;
  stopProxy(): Promise<void>;
  toggleSystemProxy(): Promise<void>;
  clear(): Promise<void>;
  saveSettings(settings: ProxySettings): Promise<void>;
  setCaModalOpen(open: boolean): void;
  setError(error: string | null): void;
}

function asError(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

export const useStore = create<AppStore>((set, get) => ({
  txs: new Map(),
  order: [],
  status: { running: false, addr: null, port: null, systemProxyEnabled: false },
  settings: null,
  ca: null,
  filter: "",
  selectedId: null,
  detail: null,
  detailLoading: false,
  caModalOpen: false,
  error: null,

  async init() {
    try {
      const [status, settings, ca, history] = await Promise.all([
        api.getStatus(),
        api.getSettings(),
        api.caState(),
        api.queryTransactions({ limit: 1000 }),
      ]);
      const txs = new Map<number, TransactionSummary>();
      // History arrives newest-first; the table wants ascending ids.
      for (const s of [...history].reverse()) txs.set(s.id, s);
      set({
        status,
        settings,
        ca,
        txs,
        order: [...txs.keys()],
        caModalOpen: !ca.generated || ca.trusted === false,
      });
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  applyBatch(events) {
    const txs = new Map(get().txs);
    let order = get().order;
    let orderChanged = false;
    for (const ev of events) {
      if (
        ev.type !== "transactionStarted" &&
        ev.type !== "transactionUpdated" &&
        ev.type !== "transactionCompleted"
      ) {
        continue;
      }
      const s = ev.data;
      if (!txs.has(s.id)) {
        if (!orderChanged) {
          order = [...order];
          orderChanged = true;
        }
        order.push(s.id);
      }
      txs.set(s.id, s);
    }
    if (orderChanged && order.length > MAX_ROWS) {
      const dropped = order.splice(0, order.length - MAX_ROWS);
      for (const id of dropped) txs.delete(id);
    }
    set(orderChanged ? { txs, order } : { txs });

    // Keep an open detail pane fresh once its transaction completes.
    const { selectedId, detail } = get();
    if (
      selectedId !== null &&
      (!detail || detail.summary.state === "active") &&
      events.some(
        (e) => e.type === "transactionCompleted" && e.data.id === selectedId,
      )
    ) {
      void get().select(selectedId);
    }
  },

  setStatus(status) {
    set({ status });
  },

  setCa(ca) {
    set({ ca });
  },

  setFilter(filter) {
    set({ filter });
  },

  async select(id) {
    if (id === null) {
      set({ selectedId: null, detail: null });
      return;
    }
    set({ selectedId: id, detailLoading: true });
    try {
      const detail = await api.getTransaction(id);
      // Ignore stale responses if the selection moved on.
      if (get().selectedId === id) set({ detail, detailLoading: false });
    } catch (e) {
      set({ error: asError(e), detailLoading: false });
    }
  },

  async startProxy(port) {
    try {
      const status = await api.startProxy(port);
      set({ status, error: null });
      const settings = get().settings;
      if (port !== undefined && settings && settings.port !== port) {
        await get().saveSettings({ ...settings, port });
      }
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  async stopProxy() {
    try {
      set({ status: await api.stopProxy(), error: null });
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  async toggleSystemProxy() {
    try {
      const enabled = !get().status.systemProxyEnabled;
      set({ status: await api.setSystemProxy(enabled), error: null });
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  async clear() {
    try {
      await api.clearSession();
      set({ txs: new Map(), order: [], selectedId: null, detail: null });
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  async saveSettings(settings) {
    try {
      await api.setSettings(settings);
      set({ settings });
    } catch (e) {
      set({ error: asError(e) });
    }
  },

  setCaModalOpen(open) {
    set({ caModalOpen: open });
  },

  setError(error) {
    set({ error });
  },
}));

export function filteredOrder(
  order: number[],
  txs: Map<number, TransactionSummary>,
  filter: string,
): number[] {
  const needle = filter.trim().toLowerCase();
  if (!needle) return order;
  return order.filter((id) => {
    const s = txs.get(id);
    if (!s) return false;
    return (
      s.host.toLowerCase().includes(needle) ||
      s.path.toLowerCase().includes(needle) ||
      s.method.toLowerCase().includes(needle) ||
      String(s.status ?? "").includes(needle)
    );
  });
}
