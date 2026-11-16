use crate::proxy::monitor::ProxyRequestLog;
use rusqlite::{params, Connection};
use std::path::PathBuf;

pub fn get_proxy_db_path() -> Result<PathBuf, String> {
    let data_dir = crate::modules::account::get_data_dir()?;
    Ok(data_dir.join("proxy_logs.db"))
}

fn connect_db() -> Result<Connection, String> {
    let db_path = get_proxy_db_path()?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;

    // Enable WAL mode for better concurrency
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;

    // Set busy timeout to 10000ms: give concurrent writers enough time to resolve
    conn.pragma_update(None, "busy_timeout", 10000)
        .map_err(|e| e.to_string())?;

    // Synchronous NORMAL is faster and safe enough for WAL
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;

    // Limit WAL journal size to 64MB: SQLite automatically truncates WAL on checkpoint
    let _ = conn.pragma_update(None, "journal_size_limit", 67_108_864);

    Ok(conn)
}

pub fn init_db() -> Result<(), String> {
    // connect_db will initialize WAL mode and other pragmas
    let conn = connect_db()?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS request_logs (
            id TEXT PRIMARY KEY,
            timestamp INTEGER,
            method TEXT,
            url TEXT,
            status INTEGER,
            duration INTEGER,
            model TEXT,
            error TEXT
        )",
        [],
    )
    .map_err(|e| e.to_string())?;

    // Try to add new columns (ignore errors if they exist)
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN request_body TEXT", []);
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN response_body TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE request_logs ADD COLUMN input_tokens INTEGER",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE request_logs ADD COLUMN output_tokens INTEGER",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE request_logs ADD COLUMN cached_tokens INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN account_email TEXT", []);
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN mapped_model TEXT", []);
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN protocol TEXT", []);
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN client_ip TEXT", []);
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN username TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE request_logs ADD COLUMN in_flight_peak INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN response_source TEXT", []);

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_timestamp ON request_logs (timestamp DESC)",
        [],
    )
    .map_err(|e| e.to_string())?;

    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_response_source ON request_logs (response_source)",
        [],
    );

    // Add status index for faster stats queries
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_status ON request_logs (status)",
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

pub fn save_log(log: &ProxyRequestLog) -> Result<(), String> {
    // 遇到瞬时 SQLITE_BUSY 时进行最多 3 次轻量重试（附带递增休眠），防止突发大事务或并发清理导致丢日志
    let mut attempts = 0;
    loop {
        attempts += 1;
        match try_save_log(log) {
            Ok(()) => return Ok(()),
            Err(e) if (e.contains("database is locked") || e.contains("code: 5") || e.contains("busy")) && attempts < 3 => {
                std::thread::sleep(std::time::Duration::from_millis(attempts * 25));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

fn try_save_log(log: &ProxyRequestLog) -> Result<(), String> {
    let conn = connect_db()?;

    // Do NOT truncate request/response bodies: preserve 100% full payloads for audit.
    // Capacity management is enforced dynamically by frequent sliding-window cleanup (pruning oldest success logs).
    let req_body = log.request_body.clone();
    let resp_body = log.response_body.clone();

    conn.execute(
        "INSERT INTO request_logs (id, timestamp, method, url, status, duration, model, error, request_body, response_body, input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak, response_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            log.id,
            log.timestamp,
            log.method,
            log.url,
            log.status,
            log.duration,
            log.model,
            log.error,
            req_body,
            resp_body,
            log.input_tokens,
            log.output_tokens,
            log.cached_tokens,
            log.account_email,
            log.mapped_model,
            log.protocol,
            log.client_ip,
            log.username,
            log.in_flight_peak,
            log.response_source,
        ],
    ).map_err(|e| e.to_string())?;

    Ok(())
}

/// Get logs summary (without large request_body and response_body fields) with pagination
pub fn get_logs_summary(limit: usize, offset: usize) -> Result<Vec<ProxyRequestLog>, String> {
    let conn = connect_db()?;

    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, method, url, status, duration, model, error,
                NULL as request_body, NULL as response_body,
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak, response_source
         FROM request_logs 
         ORDER BY timestamp DESC 
         LIMIT ?1 OFFSET ?2",
        )
        .map_err(|e| e.to_string())?;

    let logs_iter = stmt
        .query_map([limit, offset], |row| {
            Ok(ProxyRequestLog {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                method: row.get(2)?,
                url: row.get(3)?,
                status: row.get(4)?,
                duration: row.get(5)?,
                model: row.get(6)?,
                error: row.get(7)?,
                request_body: None,  // Don't query large fields for list view
                response_body: None, // Don't query large fields for list view
                input_tokens: row.get(10).unwrap_or(None),
                output_tokens: row.get(11).unwrap_or(None),
                cached_tokens: row.get(12).unwrap_or(None),
                account_email: row.get(13).unwrap_or(None),
                account_id: None,
                mapped_model: row.get(14).unwrap_or(None),
                protocol: row.get(15).unwrap_or(None),
                client_ip: row.get(16).unwrap_or(None),
                username: row.get(17).unwrap_or(None),
                in_flight_peak: row.get(18).unwrap_or(None),
                response_source: row.get(19).unwrap_or(None),
            })
        })
        .map_err(|e| e.to_string())?;

    let mut logs = Vec::new();
    for log in logs_iter {
        logs.push(log.map_err(|e| e.to_string())?);
    }
    Ok(logs)
}

/// Get logs (backward compatible, calls get_logs_summary)
pub fn get_logs(limit: usize) -> Result<Vec<ProxyRequestLog>, String> {
    get_logs_summary(limit, 0)
}

pub fn get_stats() -> Result<crate::proxy::monitor::ProxyStats, String> {
    let conn = connect_db()?;

    // Optimized: Use single query instead of three separate queries
    // Use COALESCE to handle NULL values when table is empty (SUM returns NULL for empty set)
    let (total_requests, success_count, error_count): (u64, u64, u64) = conn
        .query_row(
            "SELECT
            COUNT(*) as total,
            COALESCE(SUM(CASE WHEN status >= 200 AND status < 400 THEN 1 ELSE 0 END), 0) as success,
            COALESCE(SUM(CASE WHEN status < 200 OR status >= 400 THEN 1 ELSE 0 END), 0) as error
         FROM request_logs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| e.to_string())?;

    Ok(crate::proxy::monitor::ProxyStats {
        total_requests,
        success_count,
        error_count,
    })
}

/// Get single log detail (with request_body and response_body)
pub fn get_log_detail(log_id: &str) -> Result<ProxyRequestLog, String> {
    let conn = connect_db()?;

    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, method, url, status, duration, model, error,
                request_body, response_body, input_tokens, output_tokens,
                cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak, response_source
         FROM request_logs
         WHERE id = ?1",
        )
        .map_err(|e| e.to_string())?;

    stmt.query_row([log_id], |row| {
        Ok(ProxyRequestLog {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            method: row.get(2)?,
            url: row.get(3)?,
            status: row.get(4)?,
            duration: row.get(5)?,
            model: row.get(6)?,
            error: row.get(7)?,
            request_body: row.get(8).unwrap_or(None),
            response_body: row.get(9).unwrap_or(None),
            input_tokens: row.get(10).unwrap_or(None),
            output_tokens: row.get(11).unwrap_or(None),
            cached_tokens: row.get(12).unwrap_or(None),
            account_email: row.get(13).unwrap_or(None),
            account_id: None,
            mapped_model: row.get(14).unwrap_or(None),
            protocol: row.get(15).unwrap_or(None),
            client_ip: row.get(16).unwrap_or(None),
            username: row.get(17).unwrap_or(None),
            in_flight_peak: row.get(18).unwrap_or(None),
            response_source: row.get(19).unwrap_or(None),
        })
    })
    .map_err(|e| e.to_string())
}

/// Tiered log retention constants:
/// 不截断 request/response 报文，通过高频紧凑的滑动窗口将数据量恒定在 ~200MB 以内，
/// 依赖 SQLite Free-list 原生页复用，彻底避免全库锁死。
const SUCCESS_MAX_AGE_HOURS: i64 = 4;
const SUCCESS_MAX_ROWS: usize = 500; // 保持最新 500 条完整大包成功日志（约 150MB~250MB）

const ERROR_MAX_AGE_DAYS: i64 = 7;
const ERROR_MAX_ROWS: usize = 2_000; // 保持最新 2,000 条错误诊断日志

/// Soft target size for proxy_logs.db: if exceeded, prune oldest success logs progressively
const DB_TARGET_SIZE_BYTES: u64 = 400 * 1024 * 1024; // 400 MB soft limit
const DB_FUSE_SIZE_BYTES: u64 = 800 * 1024 * 1024; // 800 MB hard circuit-breaker

/// Get total physical size on disk of proxy_logs.db (+ wal/shm)
pub fn get_proxy_db_disk_size() -> Result<u64, String> {
    let db_path = get_proxy_db_path()?;
    let mut total: u64 = 0;
    if let Ok(meta) = std::fs::metadata(&db_path) {
        total += meta.len();
    }
    let wal = db_path.with_extension("db-wal");
    if let Ok(meta) = std::fs::metadata(&wal) {
        total += meta.len();
    }
    let shm = db_path.with_extension("db-shm");
    if let Ok(meta) = std::fs::metadata(&shm) {
        total += meta.len();
    }
    Ok(total)
}

/// Free bytes on the filesystem hosting the proxy DB (used before VACUUM,
/// which needs ~2x the DB size in temporary space).
fn free_disk_bytes() -> u64 {
    #[cfg(unix)]
    {
        let dir = get_proxy_db_path()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"));
        if let Ok(c_path) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) {
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0 {
                return stat.f_bavail as u64 * stat.f_frsize as u64;
            }
        }
    }
    #[cfg(not(unix))]
    {
        return 0;
    }
    0
}

/// Delete in small batches so the SQLite write lock is never held for long.
/// Uses standard SQLite `rowid IN (SELECT ... LIMIT)` (compatible with bundled builds
/// compiled without `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`).
fn delete_batched(
    conn: &Connection,
    status_clause: &str,
    cutoff_ts: i64,
    batch: usize,
) -> usize {
    let sql = format!(
        "DELETE FROM request_logs WHERE rowid IN (
            SELECT rowid FROM request_logs
            WHERE {} AND timestamp < ?1
            ORDER BY timestamp ASC
            LIMIT ?2
        )",
        status_clause
    );
    let mut total = 0;
    loop {
        let deleted = match conn.execute(&sql, rusqlite::params![cutoff_ts, batch as i64]) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("delete_batched failed ({}): {}", status_clause, e);
                break;
            }
        };
        if deleted == 0 {
            break;
        }
        total += deleted;
        // Don't hammer in a tight loop forever; stop if a huge backlog exists
        // (will be handled by the next periodic pass).
        if total > 2_000_000 {
            break;
        }
    }
    total
}

/// Delete all but the newest `keep` rows of a status class, using the
/// index-backed timestamp of the Nth-newest row as cutoff (no NOT IN scan).
fn delete_beyond_keep(
    conn: &Connection,
    status_clause: &str,
    keep: usize,
    batch: usize,
) -> usize {
    if keep == 0 {
        return delete_batched(conn, status_clause, i64::MAX, batch);
    }
    let lookup = format!(
        "SELECT timestamp FROM request_logs WHERE {} ORDER BY timestamp DESC LIMIT 1 OFFSET ?1",
        status_clause
    );
    let offset = (keep - 1) as i64;
    let cutoff: Option<i64> = conn.query_row(&lookup, [offset], |r| r.get(0)).unwrap_or(None);
    match cutoff {
        Some(ts) => delete_batched(conn, status_clause, ts, batch),
        None => 0,
    }
}

/// Tiered cleanup: aggressively prune transient 200 OK logs, while
/// safeguarding precious 4xx/5xx error diagnostics for up to 30 days / 50k rows.
/// All deletes run in small batches to never hold the SQLite write lock long
/// (previously a single giant NOT-IN DELETE + VACUUM starved concurrent
/// save_log writers on high-traffic nodes and hung the service).
pub fn cleanup_tiered_logs() -> Result<usize, String> {
    let conn = connect_db()?;
    let size_bytes = get_proxy_db_disk_size().ok();
    cleanup_tiered_logs_with_conn(&conn, size_bytes, free_disk_bytes())
}

fn cleanup_tiered_logs_with_conn(
    conn: &Connection,
    size_bytes: Option<u64>,
    free_disk: u64,
) -> Result<usize, String> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut total_deleted = 0;

    const BATCH: usize = 2000;
    const SUCCESS_CLAUSE: &str = "status >= 200 AND status < 400";
    const ERROR_CLAUSE: &str = "(status < 200 OR status >= 400)";

    // 1. Prune success logs older than 4 hours (batched)
    let success_cutoff = now_ms - (SUCCESS_MAX_AGE_HOURS * 3600 * 1000);
    total_deleted += delete_batched(conn, SUCCESS_CLAUSE, success_cutoff, BATCH);

    // 2. Keep newest SUCCESS_MAX_ROWS success logs (index-cutoff, batched)
    total_deleted += delete_beyond_keep(conn, SUCCESS_CLAUSE, SUCCESS_MAX_ROWS, BATCH);

    // 3. Prune error logs older than 7 days (batched)
    let error_cutoff = now_ms - (ERROR_MAX_AGE_DAYS * 24 * 3600 * 1000);
    total_deleted += delete_batched(conn, ERROR_CLAUSE, error_cutoff, BATCH);

    // 4. Keep newest ERROR_MAX_ROWS error logs (index-cutoff, batched)
    total_deleted += delete_beyond_keep(conn, ERROR_CLAUSE, ERROR_MAX_ROWS, BATCH);

    // 5. Capacity adaptive management:
    // If DB size exceeds target threshold, progressively trim oldest success logs.
    // Normal maintenance avoids VACUUM and relies on Free-list page reuse + wal_checkpoint(TRUNCATE);
    // only after a hard fuse trip with >=128 MB reclaimable free pages do we run VACUUM once
    // (when sufficient free disk space is verified) so physical file size drops below fuse threshold.
    if let Some(size) = size_bytes {
        if size > DB_FUSE_SIZE_BYTES {
            tracing::warn!(
                "Proxy DB size ({} MB) exceeded fuse threshold. Pruning oldest success logs to relieve pressure...",
                size / 1024 / 1024
            );
            let emergency_success_cutoff = now_ms - (1 * 3600 * 1000);
            total_deleted += delete_batched(conn, SUCCESS_CLAUSE, emergency_success_cutoff, BATCH);
            total_deleted += delete_beyond_keep(conn, SUCCESS_CLAUSE, 200, BATCH);
            total_deleted += delete_beyond_keep(conn, ERROR_CLAUSE, 500, BATCH);

            let freelist_pages: u64 = conn
                .query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0)
                .max(0) as u64;
            let page_size: u64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
                .unwrap_or(4096)
                .max(0) as u64;
            let reclaimable_bytes = freelist_pages.saturating_mul(page_size);

            if reclaimable_bytes >= 128 * 1024 * 1024
                && (free_disk == 0 || free_disk > size.saturating_mul(2))
            {
                tracing::info!(
                    "Proxy DB fuse recovery: compacting {} MB of reclaimable free pages via VACUUM",
                    reclaimable_bytes / 1024 / 1024
                );
                if let Err(e) = conn.execute("VACUUM", []) {
                    tracing::warn!("Proxy DB post-fuse VACUUM skipped/failed: {}", e);
                }
            }
        } else if size > DB_TARGET_SIZE_BYTES {
            // Prune down to 300 most recent success logs
            total_deleted += delete_beyond_keep(conn, SUCCESS_CLAUSE, 300, BATCH);
        }
    }

    // 每次清理后执行非阻塞的 WAL 检查点截断，立即释放预写日志磁盘占用
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");

    Ok(total_deleted)
}

/// Cleanup old logs (backward compatible wrapper for legacy calls)
pub fn cleanup_old_logs(_days: i64) -> Result<usize, String> {
    cleanup_tiered_logs()
}

/// Limit maximum log count (keep newest N records)
#[allow(dead_code)]
pub fn limit_max_logs(max_count: usize) -> Result<usize, String> {
    let conn = connect_db()?;

    let deleted = conn
        .execute(
            "DELETE FROM request_logs WHERE id NOT IN (
            SELECT id FROM request_logs ORDER BY timestamp DESC LIMIT ?1
        )",
            [max_count],
        )
        .map_err(|e| e.to_string())?;

    conn.execute("VACUUM", []).map_err(|e| e.to_string())?;

    Ok(deleted)
}

pub fn clear_logs() -> Result<(), String> {
    // 大库清空优先采用 DROP TABLE + init_db：对已膨胀至数百MB或GB级的大库
    // 比逐行 DELETE 快数万倍，且避免锁死。但 DROP 需要写锁，高流量节点上
    // 可能被并发写入/后台 VACUUM 短暂阻塞：加大 busy_timeout 并多轮重试退避。
    let conn = connect_db()?;
    conn.pragma_update(None, "busy_timeout", 8_000)
        .map_err(|e| e.to_string())?;

    let mut last_err: Option<String> = None;
    for attempt in 0..30i64 {
        match conn.execute("DROP TABLE IF EXISTS request_logs", []) {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                let is_locked = msg.contains("locked") || msg.contains("busy");
                if !is_locked {
                    return Err(format!("Failed to drop request_logs table: {}", msg));
                }
                last_err = Some(msg);
                // 退避重试：0.5s → 0.8s → 1.1s ... 封顶 ~2s，最多等 ~8s*30 轮
                tracing::warn!(
                    "clear_logs: request_logs DROP 被锁 (attempt {}), 退避重试",
                    attempt + 1
                );
                std::thread::sleep(std::time::Duration::from_millis(
                    500 + 300 * (attempt % 30) as u64,
                ));
            }
        }
    }
    if let Some(e) = last_err {
        return Err(format!("Failed to drop request_logs table: {}", e));
    }

    init_db()?;
    // 强制截断 WAL 预写日志文件，彻底释放磁盘 (仍可能被并发读占用，失败可忽略)
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
    // 释放未使用的空间；同样可能被占用，尽力而为
    for _ in 0..6 {
        if conn.execute("VACUUM", []).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    Ok(())
}

/// Get total count of logs in database
pub fn get_logs_count() -> Result<u64, String> {
    let conn = connect_db()?;

    let count: u64 = conn
        .query_row("SELECT COUNT(*) FROM request_logs", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;

    Ok(count)
}

/// Get count of logs matching search filter and source filter
/// filter: search text to match in url, method, model, or status
/// errors_only: if true, only count logs with status < 200 or >= 400
/// source_filter: optional "all", "remote", or "local"
pub fn get_logs_count_filtered(
    filter: &str,
    errors_only: bool,
    source_filter: Option<&str>,
) -> Result<u64, String> {
    let conn = connect_db()?;

    let mut conditions = Vec::new();
    let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();

    if errors_only {
        conditions.push("(status < 200 OR status >= 400)".to_string());
    }

    if !filter.is_empty() {
        conditions.push("(url LIKE ? OR method LIKE ? OR model LIKE ? OR CAST(status AS TEXT) LIKE ? OR account_email LIKE ? OR client_ip LIKE ?)".to_string());
        let pattern = format!("%{}%", filter);
        for _ in 0..6 {
            params_vec.push(rusqlite::types::Value::Text(pattern.clone()));
        }
    }

    if let Some(src) = source_filter {
        match src {
            "remote" => {
                conditions.push("response_source = 'remote'".to_string());
            }
            "local" => {
                conditions.push("(response_source = 'local' OR (response_source IS NULL AND (status < 200 OR status >= 400)))".to_string());
            }
            _ => {}
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!("SELECT COUNT(*) FROM request_logs {}", where_clause);
    let count: u64 = conn
        .query_row(&sql, rusqlite::params_from_iter(&params_vec), |row| {
            row.get(0)
        })
        .map_err(|e| e.to_string())?;

    Ok(count)
}

/// Get logs with search filter, source filter, and pagination
/// filter: search text to match in url, method, model, or status
/// errors_only: if true, only return logs with status < 200 or >= 400
/// source_filter: optional "all", "remote", or "local"
pub fn get_logs_filtered(
    filter: &str,
    errors_only: bool,
    source_filter: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<ProxyRequestLog>, String> {
    let conn = connect_db()?;

    let mut conditions = Vec::new();
    let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();

    if errors_only {
        conditions.push("(status < 200 OR status >= 400)".to_string());
    }

    if !filter.is_empty() {
        conditions.push("(url LIKE ? OR method LIKE ? OR model LIKE ? OR CAST(status AS TEXT) LIKE ? OR account_email LIKE ? OR client_ip LIKE ?)".to_string());
        let pattern = format!("%{}%", filter);
        for _ in 0..6 {
            params_vec.push(rusqlite::types::Value::Text(pattern.clone()));
        }
    }

    if let Some(src) = source_filter {
        match src {
            "remote" => {
                conditions.push("response_source = 'remote'".to_string());
            }
            "local" => {
                conditions.push("(response_source = 'local' OR (response_source IS NULL AND (status < 200 OR status >= 400)))".to_string());
            }
            _ => {}
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    params_vec.push(rusqlite::types::Value::Integer(limit as i64));
    params_vec.push(rusqlite::types::Value::Integer(offset as i64));

    let sql = format!(
        "SELECT id, timestamp, method, url, status, duration, model, error,
                NULL as request_body, NULL as response_body,
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak, response_source
         FROM request_logs
         {}
         ORDER BY timestamp DESC
         LIMIT ? OFFSET ?",
        where_clause
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let logs_iter = stmt
        .query_map(rusqlite::params_from_iter(&params_vec), |row| {
            Ok(ProxyRequestLog {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                method: row.get(2)?,
                url: row.get(3)?,
                status: row.get(4)?,
                duration: row.get(5)?,
                model: row.get(6)?,
                error: row.get(7)?,
                request_body: None,
                response_body: None,
                input_tokens: row.get(10).unwrap_or(None),
                output_tokens: row.get(11).unwrap_or(None),
                cached_tokens: row.get(12).unwrap_or(None),
                account_email: row.get(13).unwrap_or(None),
                account_id: None,
                mapped_model: row.get(14).unwrap_or(None),
                protocol: row.get(15).unwrap_or(None),
                client_ip: row.get(16).unwrap_or(None),
                username: row.get(17).unwrap_or(None),
                in_flight_peak: row.get(18).unwrap_or(None),
                response_source: row.get(19).unwrap_or(None),
            })
        })
        .map_err(|e| e.to_string())?;

    let mut logs = Vec::new();
    for log in logs_iter {
        if let Ok(l) = log {
            logs.push(l);
        }
    }

    Ok(logs)
}

/// Get all logs with full details for export
pub fn get_all_logs_for_export() -> Result<Vec<ProxyRequestLog>, String> {
    let conn = connect_db()?;

    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, method, url, status, duration, model, error,
                request_body, response_body, input_tokens, output_tokens,
                cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak, response_source
         FROM request_logs
         ORDER BY timestamp DESC",
        )
        .map_err(|e| e.to_string())?;

    let logs_iter = stmt
        .query_map([], |row| {
            Ok(ProxyRequestLog {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                method: row.get(2)?,
                url: row.get(3)?,
                status: row.get(4)?,
                duration: row.get(5)?,
                model: row.get(6)?,
                error: row.get(7)?,
                request_body: row.get(8).unwrap_or(None),
                response_body: row.get(9).unwrap_or(None),
                input_tokens: row.get(10).unwrap_or(None),
                output_tokens: row.get(11).unwrap_or(None),
                cached_tokens: row.get(12).unwrap_or(None),
                account_email: row.get(13).unwrap_or(None),
                account_id: None,
                mapped_model: row.get(14).unwrap_or(None),
                protocol: row.get(15).unwrap_or(None),
                client_ip: row.get(16).unwrap_or(None),
                username: row.get(17).unwrap_or(None),
                in_flight_peak: row.get(18).unwrap_or(None),
                response_source: row.get(19).unwrap_or(None),
            })
        })
        .map_err(|e| e.to_string())?;

    let mut logs = Vec::new();
    for log in logs_iter {
        logs.push(log.map_err(|e| e.to_string())?);
    }
    Ok(logs)
}

// ... existing code ...

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IpTokenStats {
    pub client_ip: String,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub request_count: i64,
    pub username: Option<String>,
}

/// Get token usage grouped by IP
pub fn get_token_usage_by_ip(limit: usize, hours: i64) -> Result<Vec<IpTokenStats>, String> {
    let conn = connect_db()?;

    // Fix: Database stores timestamp in milliseconds, but we were calculating 'since' in seconds
    // Convert 'hours' to milliseconds
    let since = chrono::Utc::now().timestamp_millis() - (hours * 3600 * 1000);

    // [FIX] 不再从 request_logs 表获取 username，因为该字段可能为空
    // 先获取 IP 统计数据，然后再单独查询每个 IP 的用户名
    let mut stmt = conn
        .prepare(
            "SELECT
            client_ip,
            COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0) as total,
            COALESCE(SUM(input_tokens), 0) as input,
            COALESCE(SUM(output_tokens), 0) as output,
            COUNT(*) as cnt
         FROM request_logs
         WHERE timestamp >= ?1 AND client_ip IS NOT NULL AND client_ip != ''
         GROUP BY client_ip
         ORDER BY total DESC
         LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(params![since, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut stats = Vec::new();
    for row in rows {
        let (client_ip, total_tokens, input_tokens, output_tokens, request_count) =
            row.map_err(|e| e.to_string())?;

        // 从 user_token_db 获取该 IP 关联的用户名
        // 这比从 request_logs 获取更可靠，因为 token_ip_bindings 表在每次 User Token 使用时都会更新
        let username =
            crate::modules::user_token_db::get_username_for_ip(&client_ip).unwrap_or(None);

        stats.push(IpTokenStats {
            client_ip,
            total_tokens,
            input_tokens,
            output_tokens,
            request_count,
            username,
        });
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_tiered_logs_execution() {
        // 验证 init_db 与 cleanup_tiered_logs 能够正常执行且不发生死锁或语法错误
        if let Ok(_) = init_db() {
            let res = cleanup_tiered_logs();
            assert!(res.is_ok(), "cleanup_tiered_logs should execute successfully: {:?}", res);
        }
    }

    #[test]
    fn test_cleanup_tiered_logs_actually_deletes_stale_and_excess_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE request_logs (
                id TEXT PRIMARY KEY,
                timestamp INTEGER,
                method TEXT,
                url TEXT,
                status INTEGER,
                duration INTEGER,
                model TEXT,
                error TEXT
            )",
            [],
        )
        .unwrap();

        let now_ms = chrono::Utc::now().timestamp_millis();
        let stale_success_ts = now_ms - (6 * 3600 * 1000); // > 4h
        let recent_ts = now_ms - (10 * 60 * 1000); // 10m ago

        for i in 0..5 {
            conn.execute(
                "INSERT INTO request_logs (id, timestamp, method, url, status, duration) VALUES (?1, ?2, 'POST', '/v1/messages', 200, 10)",
                rusqlite::params![format!("stale-200-{}", i), stale_success_ts - i],
            )
            .unwrap();
        }
        for i in 0..505 {
            conn.execute(
                "INSERT INTO request_logs (id, timestamp, method, url, status, duration) VALUES (?1, ?2, 'POST', '/v1/messages', 200, 10)",
                rusqlite::params![format!("recent-200-{}", i), recent_ts - i],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO request_logs (id, timestamp, method, url, status, duration, error) VALUES ('err-1', ?1, 'POST', '/v1/messages', 503, 5, 'fail')",
            rusqlite::params![stale_success_ts],
        )
        .unwrap();

        let deleted = cleanup_tiered_logs_with_conn(&conn, None, 0).unwrap();
        assert_eq!(deleted, 10, "5 stale (>4h) + 5 excess (>500) 200 rows must be deleted");

        let ok_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM request_logs WHERE status = 200",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let err_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM request_logs WHERE status = 503",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ok_left, 500);
        assert_eq!(err_left, 1, "error rows within 7d retention must be preserved");
    }
}

