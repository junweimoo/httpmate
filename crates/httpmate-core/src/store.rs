//! SQLite-backed transaction store.
//!
//! A single OS thread owns the connection; the async side talks to it over a
//! channel, so the proxy hot path never blocks on disk. Completed
//! transactions are written here; the live UI view is fed by bus events and
//! only comes back to the store for history and detail lookups.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result};
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::oneshot;

use crate::events::{QueryFilter, TransactionDetail, TransactionSummary, TxState};

/// Bodies up to this size live in the `bodies` table; larger ones spill to
/// files under `<data_dir>/blobs/`.
const INLINE_BODY_LIMIT: usize = 256 * 1024;
/// Retention cap, enforced by background pruning.
const MAX_TRANSACTIONS: u64 = 50_000;
const PRUNE_EVERY: u64 = 500;

/// Everything known about a finished transaction, ready to persist.
#[derive(Debug)]
pub struct CompletedTx {
    pub summary: TransactionSummary,
    pub http_version: String,
    pub client_addr: String,
    pub tls_version: Option<String>,
    pub alpn: Option<String>,
    pub req_header_blob: Vec<u8>,
    pub resp_header_blob: Vec<u8>,
    pub req_body: Vec<u8>,
    pub req_body_total: u64,
    pub req_body_truncated: bool,
    pub resp_body: Vec<u8>,
    pub resp_body_total: u64,
    pub resp_body_truncated: bool,
    pub tags: serde_json::Value,
}

enum Msg {
    Insert(Box<CompletedTx>, oneshot::Sender<Result<()>>),
    Query(QueryFilter, oneshot::Sender<Result<Vec<TransactionSummary>>>),
    Get(u64, oneshot::Sender<Result<Option<TransactionDetail>>>),
    Clear(oneshot::Sender<Result<()>>),
}

#[derive(Clone)]
pub struct StoreHandle {
    tx: mpsc::Sender<Msg>,
    pub max_id_at_open: u64,
}

impl StoreHandle {
    /// Open (creating if needed) the session database under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<StoreHandle> {
        std::fs::create_dir_all(data_dir.join("blobs"))?;
        let db_path = data_dir.join("session.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        let max_id: u64 =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM transactions", [], |r| r.get(0))?;

        let (tx, rx) = mpsc::channel::<Msg>();
        let blob_dir = data_dir.join("blobs");
        std::thread::Builder::new()
            .name("httpmate-store".into())
            .spawn(move || writer_loop(conn, rx, blob_dir))
            .context("spawning store thread")?;
        Ok(StoreHandle { tx, max_id_at_open: max_id })
    }

    pub async fn insert(&self, tx: CompletedTx) -> Result<()> {
        self.call(|reply| Msg::Insert(Box::new(tx), reply)).await?
    }

    pub async fn query(&self, filter: QueryFilter) -> Result<Vec<TransactionSummary>> {
        self.call(|reply| Msg::Query(filter, reply)).await?
    }

    pub async fn get(&self, id: u64) -> Result<Option<TransactionDetail>> {
        self.call(|reply| Msg::Get(id, reply)).await?
    }

    pub async fn clear(&self) -> Result<()> {
        self.call(Msg::Clear).await?
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make(reply_tx))
            .map_err(|_| anyhow::anyhow!("store thread is gone"))?;
        reply_rx.await.map_err(|_| anyhow::anyhow!("store thread dropped reply"))
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transactions (
  id                  INTEGER PRIMARY KEY,
  started_at          INTEGER NOT NULL,
  duration_ms         INTEGER,
  kind                TEXT NOT NULL,
  scheme              TEXT NOT NULL,
  method              TEXT NOT NULL,
  host                TEXT NOT NULL,
  path                TEXT NOT NULL,
  query               TEXT,
  http_version        TEXT NOT NULL DEFAULT '',
  status              INTEGER,
  req_header_blob     BLOB,
  resp_header_blob    BLOB,
  req_body_ref        TEXT NOT NULL DEFAULT 'none',
  resp_body_ref       TEXT NOT NULL DEFAULT 'none',
  req_body_total      INTEGER NOT NULL DEFAULT 0,
  resp_body_total     INTEGER NOT NULL DEFAULT 0,
  req_body_truncated  INTEGER NOT NULL DEFAULT 0,
  resp_body_truncated INTEGER NOT NULL DEFAULT 0,
  req_size            INTEGER NOT NULL DEFAULT 0,
  resp_size           INTEGER NOT NULL DEFAULT 0,
  client_addr         TEXT NOT NULL DEFAULT '',
  tls_version         TEXT,
  alpn                TEXT,
  content_type        TEXT,
  error               TEXT,
  tags                TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_tx_started ON transactions(started_at);
CREATE INDEX IF NOT EXISTS idx_tx_host ON transactions(host);
CREATE TABLE IF NOT EXISTS bodies (
  tx_id INTEGER NOT NULL,
  kind  TEXT NOT NULL,
  data  BLOB NOT NULL,
  PRIMARY KEY (tx_id, kind)
);
";

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Msg>, blob_dir: PathBuf) {
    let mut inserts_since_prune: u64 = 0;
    while let Ok(msg) = rx.recv() {
        let mut pending = VecDeque::from([msg]);
        // Opportunistically drain so bursts of inserts share one txn.
        while let Ok(more) = rx.try_recv() {
            pending.push_back(more);
            if pending.len() >= 256 {
                break;
            }
        }

        let mut batch: Vec<(Box<CompletedTx>, oneshot::Sender<Result<()>>)> = Vec::new();
        let mut other: Vec<Msg> = Vec::new();
        for m in pending {
            match m {
                Msg::Insert(tx, reply) => batch.push((tx, reply)),
                m => other.push(m),
            }
        }

        if !batch.is_empty() {
            let result = insert_batch(&mut conn, &blob_dir, &batch);
            inserts_since_prune += batch.len() as u64;
            for (_, reply) in batch {
                let _ = reply.send(result.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}")));
            }
            if inserts_since_prune >= PRUNE_EVERY {
                inserts_since_prune = 0;
                if let Err(e) = prune(&conn, &blob_dir) {
                    tracing::warn!("prune failed: {e:#}");
                }
            }
        }

        for m in other {
            match m {
                Msg::Insert(..) => unreachable!(),
                Msg::Query(filter, reply) => {
                    let _ = reply.send(query(&conn, &filter));
                }
                Msg::Get(id, reply) => {
                    let _ = reply.send(get(&conn, &blob_dir, id));
                }
                Msg::Clear(reply) => {
                    let _ = reply.send(clear(&conn, &blob_dir));
                }
            }
        }
    }
}

fn body_ref_and_store(
    txn: &rusqlite::Transaction,
    blob_dir: &Path,
    id: u64,
    kind: &str,
    body: &[u8],
) -> Result<String> {
    if body.is_empty() {
        return Ok("none".into());
    }
    if body.len() <= INLINE_BODY_LIMIT {
        txn.execute(
            "INSERT OR REPLACE INTO bodies (tx_id, kind, data) VALUES (?1, ?2, ?3)",
            params![id, kind, body],
        )?;
        Ok("inline".into())
    } else {
        let name = format!("{id}.{kind}");
        std::fs::write(blob_dir.join(&name), body)?;
        Ok(format!("file:{name}"))
    }
}

fn insert_batch(
    conn: &mut Connection,
    blob_dir: &Path,
    batch: &[(Box<CompletedTx>, oneshot::Sender<Result<()>>)],
) -> Result<()> {
    let txn = conn.transaction()?;
    for (tx, _) in batch {
        let s = &tx.summary;
        let req_ref = body_ref_and_store(&txn, blob_dir, s.id, "req", &tx.req_body)?;
        let resp_ref = body_ref_and_store(&txn, blob_dir, s.id, "resp", &tx.resp_body)?;
        txn.execute(
            "INSERT OR REPLACE INTO transactions (
                id, started_at, duration_ms, kind, scheme, method, host, path, query,
                http_version, status, req_header_blob, resp_header_blob,
                req_body_ref, resp_body_ref, req_body_total, resp_body_total,
                req_body_truncated, resp_body_truncated, req_size, resp_size,
                client_addr, tls_version, alpn, content_type, error, tags
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
            params![
                s.id,
                s.started_at_ms,
                s.duration_ms,
                s.kind,
                s.scheme,
                s.method,
                s.host,
                s.path,
                s.query,
                tx.http_version,
                s.status,
                tx.req_header_blob,
                tx.resp_header_blob,
                req_ref,
                resp_ref,
                tx.req_body_total,
                tx.resp_body_total,
                tx.req_body_truncated,
                tx.resp_body_truncated,
                s.req_size,
                s.resp_size,
                tx.client_addr,
                tx.tls_version,
                tx.alpn,
                s.content_type,
                s.error,
                serde_json::to_string(&tx.tags)?,
            ],
        )?;
    }
    txn.commit()?;
    Ok(())
}

const SUMMARY_COLS: &str = "id, started_at, duration_ms, kind, scheme, method, host, path, query, \
                            status, req_size, resp_size, content_type, error";

fn row_to_summary(row: &rusqlite::Row) -> rusqlite::Result<TransactionSummary> {
    let error: Option<String> = row.get(13)?;
    Ok(TransactionSummary {
        id: row.get(0)?,
        started_at_ms: row.get(1)?,
        duration_ms: row.get(2)?,
        kind: row.get(3)?,
        scheme: row.get(4)?,
        method: row.get(5)?,
        host: row.get(6)?,
        path: row.get(7)?,
        query: row.get(8)?,
        status: row.get(9)?,
        req_size: row.get(10)?,
        resp_size: row.get(11)?,
        content_type: row.get(12)?,
        state: if error.is_some() { TxState::Failed } else { TxState::Completed },
        error,
    })
}

fn query(conn: &Connection, filter: &QueryFilter) -> Result<Vec<TransactionSummary>> {
    let mut sql = format!("SELECT {SUMMARY_COLS} FROM transactions WHERE 1=1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(search) = filter.search.as_deref().filter(|s| !s.is_empty()) {
        sql.push_str(" AND (host LIKE ?1 OR path LIKE ?1)");
        args.push(format!("%{search}%").into());
    }
    if let Some(host) = &filter.host {
        args.push(host.clone().into());
        sql.push_str(&format!(" AND host = ?{}", args.len()));
    }
    if let Some(method) = &filter.method {
        args.push(method.clone().into());
        sql.push_str(&format!(" AND method = ?{}", args.len()));
    }
    if let Some(status) = filter.status {
        args.push((status as i64).into());
        sql.push_str(&format!(" AND status = ?{}", args.len()));
    }
    if let Some(before) = filter.before_id {
        args.push((before as i64).into());
        sql.push_str(&format!(" AND id < ?{}", args.len()));
    }
    sql.push_str(" ORDER BY id DESC LIMIT ");
    sql.push_str(&filter.limit.unwrap_or(500).min(5000).to_string());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_summary)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn load_body(conn: &Connection, blob_dir: &Path, id: u64, kind: &str, body_ref: &str) -> Result<Vec<u8>> {
    match body_ref {
        "none" => Ok(Vec::new()),
        "inline" => {
            let data: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT data FROM bodies WHERE tx_id = ?1 AND kind = ?2",
                    params![id, kind],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(data.unwrap_or_default())
        }
        r if r.starts_with("file:") => {
            Ok(std::fs::read(blob_dir.join(&r[5..])).unwrap_or_default())
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_header_blob(blob: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(blob)
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(": ")?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

fn get(conn: &Connection, blob_dir: &Path, id: u64) -> Result<Option<TransactionDetail>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {SUMMARY_COLS}, http_version, client_addr, tls_version, alpn, \
                 req_header_blob, resp_header_blob, req_body_ref, resp_body_ref, \
                 req_body_total, resp_body_total, req_body_truncated, resp_body_truncated, tags \
                 FROM transactions WHERE id = ?1"
            ),
            params![id],
            |row| {
                let summary = row_to_summary(row)?;
                Ok((
                    summary,
                    row.get::<_, String>(14)?,         // http_version
                    row.get::<_, String>(15)?,         // client_addr
                    row.get::<_, Option<String>>(16)?, // tls_version
                    row.get::<_, Option<String>>(17)?, // alpn
                    row.get::<_, Option<Vec<u8>>>(18)?,
                    row.get::<_, Option<Vec<u8>>>(19)?,
                    row.get::<_, String>(20)?,
                    row.get::<_, String>(21)?,
                    row.get::<_, u64>(22)?,
                    row.get::<_, u64>(23)?,
                    row.get::<_, bool>(24)?,
                    row.get::<_, bool>(25)?,
                    row.get::<_, String>(26)?,
                ))
            },
        )
        .optional()?;

    let Some((
        summary,
        http_version,
        client_addr,
        tls_version,
        alpn,
        req_hdr,
        resp_hdr,
        req_ref,
        resp_ref,
        req_total,
        resp_total,
        req_trunc,
        resp_trunc,
        tags,
    )) = row
    else {
        return Ok(None);
    };

    let b64 = base64::engine::general_purpose::STANDARD;
    let req_body = load_body(conn, blob_dir, id, "req", &req_ref)?;
    let resp_body = load_body(conn, blob_dir, id, "resp", &resp_ref)?;
    Ok(Some(TransactionDetail {
        summary,
        http_version,
        client_addr,
        tls_version,
        alpn,
        req_headers: parse_header_blob(req_hdr.as_deref().unwrap_or_default()),
        resp_headers: parse_header_blob(resp_hdr.as_deref().unwrap_or_default()),
        req_body_base64: b64.encode(&req_body),
        req_body_truncated: req_trunc,
        req_body_total: req_total,
        resp_body_base64: b64.encode(&resp_body),
        resp_body_truncated: resp_trunc,
        resp_body_total: resp_total,
        tags: serde_json::from_str(&tags).unwrap_or(serde_json::Value::Null),
    }))
}

fn clear(conn: &Connection, blob_dir: &Path) -> Result<()> {
    conn.execute("DELETE FROM transactions", [])?;
    conn.execute("DELETE FROM bodies", [])?;
    if let Ok(entries) = std::fs::read_dir(blob_dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn prune(conn: &Connection, blob_dir: &Path) -> Result<()> {
    let min_kept: Option<u64> = conn
        .query_row(
            "SELECT MIN(id) FROM (SELECT id FROM transactions ORDER BY id DESC LIMIT ?1)",
            params![MAX_TRANSACTIONS],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(min_kept) = min_kept else { return Ok(()) };
    let deleted = conn.execute("DELETE FROM transactions WHERE id < ?1", params![min_kept])?;
    if deleted == 0 {
        return Ok(());
    }
    conn.execute("DELETE FROM bodies WHERE tx_id < ?1", params![min_kept])?;
    if let Ok(entries) = std::fs::read_dir(blob_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some((id, _)) = name.split_once('.') {
                if id.parse::<u64>().map(|id| id < min_kept).unwrap_or(false) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    tracing::debug!("pruned {deleted} transactions below id {min_kept}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: u64, host: &str, body: Vec<u8>) -> CompletedTx {
        CompletedTx {
            summary: TransactionSummary {
                id,
                started_at_ms: 1_000 + id as i64,
                kind: "http".into(),
                scheme: "https".into(),
                method: "GET".into(),
                host: host.into(),
                path: "/api/items".into(),
                query: Some("page=1".into()),
                status: Some(200),
                duration_ms: Some(12),
                req_size: 0,
                resp_size: body.len() as u64,
                content_type: Some("application/json".into()),
                error: None,
                state: TxState::Completed,
            },
            http_version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:55555".into(),
            tls_version: Some("TLSv1.3".into()),
            alpn: Some("h2".into()),
            req_header_blob: b"accept: application/json\nhost: example.com\n".to_vec(),
            resp_header_blob: b"content-type: application/json\n".to_vec(),
            req_body: Vec::new(),
            req_body_total: 0,
            req_body_truncated: false,
            resp_body_total: body.len() as u64,
            resp_body: body,
            resp_body_truncated: false,
            tags: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn roundtrip_inline_and_spilled_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreHandle::open(dir.path()).unwrap();
        assert_eq!(store.max_id_at_open, 0);

        store.insert(sample(1, "example.com", b"{\"ok\":true}".to_vec())).await.unwrap();
        let big = vec![0xAB_u8; INLINE_BODY_LIMIT + 1024];
        store.insert(sample(2, "big.example.com", big.clone())).await.unwrap();

        let d1 = store.get(1).await.unwrap().unwrap();
        assert_eq!(d1.summary.host, "example.com");
        assert_eq!(d1.req_headers[0], ("accept".into(), "application/json".into()));
        let b64 = base64::engine::general_purpose::STANDARD;
        assert_eq!(b64.decode(d1.resp_body_base64).unwrap(), b"{\"ok\":true}");

        let d2 = store.get(2).await.unwrap().unwrap();
        assert_eq!(b64.decode(d2.resp_body_base64).unwrap(), big);
        assert!(dir.path().join("blobs/2.resp").exists(), "big body should spill to a file");

        assert!(store.get(99).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn query_filters_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreHandle::open(dir.path()).unwrap();
        for i in 1..=5 {
            let host = if i % 2 == 0 { "even.test" } else { "odd.test" };
            store.insert(sample(i, host, vec![])).await.unwrap();
        }

        let all = store.query(QueryFilter::default()).await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].id, 5, "newest first");

        let evens = store
            .query(QueryFilter { host: Some("even.test".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(evens.len(), 2);

        let searched = store
            .query(QueryFilter { search: Some("odd".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(searched.len(), 3);

        let paged = store
            .query(QueryFilter { before_id: Some(3), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(paged.len(), 2);

        store.clear().await.unwrap();
        assert!(store.query(QueryFilter::default()).await.unwrap().is_empty());

        // Reopen sees persisted state (empty after clear).
        drop(store);
        let store2 = StoreHandle::open(dir.path()).unwrap();
        assert_eq!(store2.max_id_at_open, 0);
    }
}
