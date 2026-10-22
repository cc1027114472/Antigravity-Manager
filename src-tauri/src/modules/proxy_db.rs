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

    // Set busy timeout to 1000ms: on a busy node, concurrent writers should
    // fail fast and drop the log rather than pile up in the blocking pool
    // (a 5s timeout under lock contention starved the spawn_blocking pool
    // and hung the whole service on high-traffic nodes).
    conn.pragma_update(None, "busy_timeout", 1000)
        .map_err(|e| e.to_string())?;

    // Synchronous NORMAL is faster and safe enough for WAL
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;

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

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_timestamp ON request_logs (timestamp DESC)",
        [],
    )
    .map_err(|e| e.to_string())?;

    // Add status index for faster stats queries
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_status ON request_logs (status)",
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

pub fn save_log(log: &ProxyRequestLog) -> Result<(), String> {
    let conn = connect_db()?;

    // Do NOT truncate request/response bodies: preserve 100% full payloads for audit.
    // Capacity management is enforced dynamically by frequent sliding-window cleanup (pruning oldest success logs).
    let req_body = log.request_body.clone();
    let resp_body = log.response_body.clone();

    conn.execute(
        "INSERT INTO request_logs (id, timestamp, method, url, status, duration, model, error, request_body, response_body, input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
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
                cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
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
        })
    })
    .map_err(|e| e.to_string())
}

/// Tiered log retention constants
const SUCCESS_MAX_AGE_HOURS: i64 = 12;
const SUCCESS_MAX_ROWS: usize = 3_000;

const ERROR_MAX_AGE_DAYS: i64 = 30;
const ERROR_MAX_ROWS: usize = 50_000;

/// Soft target size for proxy_logs.db: if exceeded, prune oldest success logs progressively
const DB_TARGET_SIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB soft limit
const DB_FUSE_SIZE_BYTES: u64 = 1536 * 1024 * 1024; // 1.5 GB hard circuit-breaker

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
/// Uses the `idx_timestamp` index (timestamp-cutoff style), never NOT IN full scans.
fn delete_batched(
    conn: &Connection,
    status_clause: &str,
    cutoff_ts: i64,
    batch: usize,
) -> usize {
    let sql = format!(
        "DELETE FROM request_logs WHERE {} AND timestamp < ?1 LIMIT ?2",
        status_clause
    );
    let mut total = 0;
    loop {
        let deleted = conn.execute(&sql, rusqlite::params![cutoff_ts, batch]).unwrap_or(0);
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
    let lookup = format!(
        "SELECT timestamp FROM request_logs WHERE {} ORDER BY timestamp DESC LIMIT 1 OFFSET ?1",
        status_clause
    );
    let cutoff: Option<i64> = conn.query_row(&lookup, [keep], |r| r.get(0)).unwrap_or(None);
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut total_deleted = 0;

    const BATCH: usize = 2000;
    const SUCCESS_CLAUSE: &str = "status >= 200 AND status < 400";
    const ERROR_CLAUSE: &str = "(status < 200 OR status >= 400)";

    // 1. Prune success logs older than 12 hours (batched)
    let success_cutoff = now_ms - (SUCCESS_MAX_AGE_HOURS * 3600 * 1000);
    total_deleted += delete_batched(&conn, SUCCESS_CLAUSE, success_cutoff, BATCH);

    // 2. Keep newest SUCCESS_MAX_ROWS success logs (index-cutoff, batched)
    total_deleted += delete_beyond_keep(&conn, SUCCESS_CLAUSE, SUCCESS_MAX_ROWS, BATCH);

    // 3. Prune error logs older than 30 days (batched)
    let error_cutoff = now_ms - (ERROR_MAX_AGE_DAYS * 24 * 3600 * 1000);
    total_deleted += delete_batched(&conn, ERROR_CLAUSE, error_cutoff, BATCH);

    // 4. Keep newest ERROR_MAX_ROWS error logs (index-cutoff, batched)
    total_deleted += delete_beyond_keep(&conn, ERROR_CLAUSE, ERROR_MAX_ROWS, BATCH);

    // 5. Capacity adaptive management:
    // If DB size exceeds target threshold (500MB), progressively trim oldest success logs.
    // If DB size exceeds hard fuse (1.5GB), apply emergency pressure relief.
    if let Ok(size) = get_proxy_db_disk_size() {
        if size > DB_FUSE_SIZE_BYTES {
            tracing::warn!(
                "Proxy DB size ({} MB) exceeded 1.5GB circuit-breaker. Triggering emergency prune!",
                size / 1024 / 1024
            );
            // Emergency: only keep 2 hours of success logs, 500 success rows
            let emergency_success_cutoff = now_ms - (2 * 3600 * 1000);
            total_deleted += delete_batched(&conn, SUCCESS_CLAUSE, emergency_success_cutoff, BATCH);
            total_deleted += delete_beyond_keep(&conn, SUCCESS_CLAUSE, 500, BATCH);
            // Emergency error limit: keep newest 10,000
            total_deleted += delete_beyond_keep(&conn, ERROR_CLAUSE, 10_000, BATCH);
            // Force vacuum on emergency (only if enough free disk for the copy)
            let free_bytes = free_disk_bytes();
            if free_bytes > 2u64 * 1024 * 1024 * 1024 {
                let _ = conn.execute("VACUUM", []);
            }
        } else if size > DB_TARGET_SIZE_BYTES {
            tracing::info!(
                "Proxy DB size ({} MB) exceeded soft target ({} MB). Pruning oldest success logs to preserve disk space...",
                size / 1024 / 1024,
                DB_TARGET_SIZE_BYTES / 1024 / 1024
            );
            // Prune down to 1,000 most recent success logs
            total_deleted += delete_beyond_keep(&conn, SUCCESS_CLAUSE, 1_000, BATCH);
            let free_bytes = free_disk_bytes();
            if free_bytes > 2u64 * 1024 * 1024 * 1024 {
                let _ = conn.execute("VACUUM", []);
            }
        } else if total_deleted > 10_000 {
            // Reclaim space when a large batch of rows was pruned
            let size_mb = size / 1024 / 1024;
            if size_mb > 200 {
                let free_bytes = free_disk_bytes();
                if free_bytes > 2u64 * 1024 * 1024 * 1024 {
                    let _ = conn.execute("VACUUM", []);
                }
            }
        }
    }

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
    let conn = connect_db()?;
    conn.execute("DELETE FROM request_logs", [])
        .map_err(|e| e.to_string())?;
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

/// Get count of logs matching search filter
/// filter: search text to match in url, method, model, or status
/// errors_only: if true, only count logs with status < 200 or >= 400
pub fn get_logs_count_filtered(filter: &str, errors_only: bool) -> Result<u64, String> {
    let conn = connect_db()?;

    let filter_pattern = format!("%{}%", filter);

    let sql = if errors_only {
        "SELECT COUNT(*) FROM request_logs WHERE (status < 200 OR status >= 400)"
    } else if filter.is_empty() {
        "SELECT COUNT(*) FROM request_logs"
    } else {
        "SELECT COUNT(*) FROM request_logs WHERE
            (url LIKE ?1 OR method LIKE ?1 OR model LIKE ?1 OR CAST(status AS TEXT) LIKE ?1 OR account_email LIKE ?1)"
    };

    let count: u64 = if filter.is_empty() && !errors_only {
        conn.query_row(sql, [], |row| row.get(0))
    } else if errors_only {
        conn.query_row(sql, [], |row| row.get(0))
    } else {
        conn.query_row(sql, [&filter_pattern], |row| row.get(0))
    }
    .map_err(|e| e.to_string())?;

    Ok(count)
}

/// Get logs with search filter and pagination
/// filter: search text to match in url, method, model, or status
/// errors_only: if true, only return logs with status < 200 or >= 400
pub fn get_logs_filtered(
    filter: &str,
    errors_only: bool,
    limit: usize,
    offset: usize,
) -> Result<Vec<ProxyRequestLog>, String> {
    let conn = connect_db()?;

    let filter_pattern = format!("%{}%", filter);

    let sql = if errors_only {
        "SELECT id, timestamp, method, url, status, duration, model, error,
                NULL as request_body, NULL as response_body,
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
         FROM request_logs
         WHERE (status < 200 OR status >= 400)
         ORDER BY timestamp DESC
         LIMIT ?1 OFFSET ?2"
    } else if filter.is_empty() {
        "SELECT id, timestamp, method, url, status, duration, model, error,
                NULL as request_body, NULL as response_body,
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
         FROM request_logs
         ORDER BY timestamp DESC
         LIMIT ?1 OFFSET ?2"
    } else {
        "SELECT id, timestamp, method, url, status, duration, model, error,
                NULL as request_body, NULL as response_body,
                input_tokens, output_tokens, cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
         FROM request_logs
         WHERE (url LIKE ?3 OR method LIKE ?3 OR model LIKE ?3 OR CAST(status AS TEXT) LIKE ?3 OR account_email LIKE ?3 OR client_ip LIKE ?3)
         ORDER BY timestamp DESC
         LIMIT ?1 OFFSET ?2"
    };

    let logs: Vec<ProxyRequestLog> = if filter.is_empty() && !errors_only {
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
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
                })
            })
            .map_err(|e| e.to_string())?;
        logs_iter.filter_map(|r| r.ok()).collect()
    } else if errors_only {
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
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
                })
            })
            .map_err(|e| e.to_string())?;
        logs_iter.filter_map(|r| r.ok()).collect()
    } else {
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let logs_iter = stmt
            .query_map(rusqlite::params![limit, offset, filter_pattern], |row| {
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
                })
            })
            .map_err(|e| e.to_string())?;
        logs_iter.filter_map(|r| r.ok()).collect()
    };

    Ok(logs)
}

/// Get all logs with full details for export
pub fn get_all_logs_for_export() -> Result<Vec<ProxyRequestLog>, String> {
    let conn = connect_db()?;

    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, method, url, status, duration, model, error,
                request_body, response_body, input_tokens, output_tokens,
                cached_tokens, account_email, mapped_model, protocol, client_ip, username, in_flight_peak
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
