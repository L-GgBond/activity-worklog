use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, AllowOrigin, CorsLayer};

struct AppState {
    conn: Mutex<Connection>,
    token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DailyRecordDto {
    date: String,
    first_activity_at: Option<String>,
    last_activity_at: Option<String>,
    active_minutes: i64,
    idle_minutes: i64,
}

#[derive(Debug, Deserialize)]
struct SyncPayload {
    #[serde(default)]
    device_id: String,
    records: Vec<DailyRecordDto>,
}

/// 客户端删除某几天记录后，把日期同步过来，避免换机恢复时又把它们拉回去。
#[derive(Debug, Deserialize)]
struct DeletePayload {
    #[serde(default)]
    device_id: String,
    dates: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SegmentDto {
    date: String,
    started_at: String,
    ended_at: String,
    segment_type: String,
}

#[derive(Debug, Deserialize)]
struct SegmentPayload {
    #[serde(default)]
    device_id: String,
    segments: Vec<SegmentDto>,
}

/// 客户端设置（作息、工作日历、通知偏好等），服务端只做存储与看板展示。
#[derive(Debug, Deserialize)]
struct SettingsPayload {
    #[serde(default)]
    device_id: String,
    settings: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct PulledSegmentDto {
    device_id: String,
    date: String,
    started_at: String,
    ended_at: String,
    segment_type: String,
}

#[derive(Debug, Clone, Serialize)]
struct ReportRow {
    period: String,
    days: i64,
    active_minutes: i64,
    idle_minutes: i64,
    first_activity_at: Option<String>,
    last_activity_at: Option<String>,
}

fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("activity-worklog-server")
}

fn open_db() -> anyhow::Result<Connection> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;
    let conn = Connection::open(dir.join("server.sqlite3"))?;

    // 旧表以 date 为单主键，多设备同一天会互相覆盖。检测主键列，
    // 命中旧结构就把数据搬到 (device_id, date) 复合主键的新表。
    let pk_columns = table_pk_columns(&conn, "synced_records");
    if pk_columns.is_empty() {
        conn.execute_batch(
            "
            create table synced_records (
                device_id text not null default '',
                date text not null,
                first_activity_at text,
                last_activity_at text,
                active_minutes integer not null default 0,
                idle_minutes integer not null default 0,
                received_at text not null,
                primary key (device_id, date)
            );

            create index idx_synced_records_date on synced_records(date);
            ",
        )?;
    } else if pk_columns != vec!["device_id".to_string(), "date".to_string()] {
        conn.execute_batch(
            "
            create table synced_records_new (
                device_id text not null default '',
                date text not null,
                first_activity_at text,
                last_activity_at text,
                active_minutes integer not null default 0,
                idle_minutes integer not null default 0,
                received_at text not null,
                primary key (device_id, date)
            );

            insert or ignore into synced_records_new
                (device_id, date, first_activity_at, last_activity_at, active_minutes, idle_minutes, received_at)
            select
                device_id, date, first_activity_at, last_activity_at, active_minutes, idle_minutes, received_at
            from synced_records;

            drop table synced_records;
            alter table synced_records_new rename to synced_records;
            create index idx_synced_records_date on synced_records(date);
            ",
        )?;
    }

    // 时段明细：换机恢复时需要它，否则历史那几天只能看不能改。
    conn.execute_batch(
        "
        create table if not exists synced_segments (
            device_id text not null,
            date text not null,
            started_at text not null,
            ended_at text not null,
            segment_type text not null check(segment_type in ('work', 'away')),
            received_at text not null,
            primary key (device_id, date, started_at)
        );

        create index if not exists idx_synced_segments_date on synced_segments(date);

        create table if not exists synced_settings (
            device_id text primary key,
            settings_json text not null,
            updated_at text not null
        );
        ",
    )?;
    Ok(conn)
}

/// 读取某张表主键列的真实名称（按主键内顺序），用来判断是否需要迁移。
fn table_pk_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(&format!("pragma table_info({table})")) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let mut rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?)))
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    rows.sort_by_key(|(order, _)| *order);
    rows.into_iter()
        .filter(|(order, _)| *order > 0)
        .map(|(_, name)| name)
        .collect()
}

fn load_or_create_token() -> anyhow::Result<String> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;
    let token_path = dir.join("server-token.txt");
    if let Ok(existing) = fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            // 兼容早期生成、权限可能过宽的 Token 文件，顺手收紧
            restrict_token_file(&token_path)?;
            return Ok(trimmed.to_string());
        }
    }
    let token = generate_token()?;
    fs::write(&token_path, &token)?;
    restrict_token_file(&token_path)?;
    Ok(token)
}

/// 32 字节全部来自系统 CSPRNG（Windows 走 BCryptGenRandom，Unix 走 getrandom(2)）。
/// 取不到随机源时直接让服务启动失败，绝不退回「时间戳哈希」这种可预测值。
fn generate_token() -> anyhow::Result<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf)
        .map_err(|err| anyhow::anyhow!("系统随机源不可用：{err}"))?;
    Ok(buf.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Token 文件仅当前用户可读写（Unix）。Windows 用户配置目录默认已只有本人可访问。
#[cfg(unix)]
fn restrict_token_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_token_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn authorize(headers: &HeaderMap, token: &str) -> Result<(), StatusCode> {
    let value = headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .unwrap_or_default();
    let provided = value.strip_prefix("Bearer ").unwrap_or(value).trim();
    if !provided.is_empty() && provided == token {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// 拉取服务器上的日汇总：换机/重装客户端后可以直接补回历史（只返回日汇总，不含时段明细）。
async fn pull_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let days = query.days.unwrap_or(365).clamp(1, 3660);
    let since = date_n_days_ago(days - 1);
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = query_rows(&conn, &format!("date >= '{since}'"))?;
    let records = rows
        .into_iter()
        .map(|(date, first, last, active, idle)| {
            serde_json::json!({
                "date": date,
                "first_activity_at": first,
                "last_activity_at": last,
                "active_minutes": active,
                "idle_minutes": idle,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({ "records": records })))
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> Result<String, StatusCode> {
    authorize(&headers, &state.token)?;
    Ok("ok".to_string())
}

async fn sync_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncPayload>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let received_at = Local::now().to_rfc3339();
    // 逐条确认：返回被接受的日期列表，客户端只标记确认过的日期
    let mut accepted: Vec<String> = Vec::new();
    for record in &payload.records {
        if NaiveDate::parse_from_str(&record.date, "%Y-%m-%d").is_err() {
            continue;
        }
        conn.execute(
            "
            insert into synced_records
                (device_id, date, first_activity_at, last_activity_at, active_minutes, idle_minutes, received_at)
            values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            on conflict(device_id, date) do update set
                first_activity_at = excluded.first_activity_at,
                last_activity_at = excluded.last_activity_at,
                active_minutes = excluded.active_minutes,
                idle_minutes = excluded.idle_minutes,
                received_at = excluded.received_at
            ",
            params![
                payload.device_id,
                record.date,
                record.first_activity_at,
                record.last_activity_at,
                record.active_minutes,
                record.idle_minutes,
                received_at
            ],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        accepted.push(record.date.clone());
    }
    Ok(Json(serde_json::json!({ "accepted": accepted })))
}

/// 删除服务器上的指定日期记录：客户端本地删除后调用，保证恢复历史时不会把删掉的日期拉回来。
async fn delete_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DeletePayload>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut deleted: Vec<String> = Vec::new();
    for date in &payload.dates {
        if NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
            continue;
        }
        // 只删本设备的行：两台机器同一天各有记录时，不能连别人的一起删掉。
        let removed = conn.execute(
            "delete from synced_records where device_id = ?1 and date = ?2",
            params![payload.device_id, date],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        conn.execute(
            "delete from synced_segments where device_id = ?1 and date = ?2",
            params![payload.device_id, date],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if removed > 0 {
            deleted.push(date.clone());
        }
    }
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

/// 接收某台设备的时段明细，按 (device_id, date, started_at) 幂等覆盖。
async fn sync_segments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SegmentPayload>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let received_at = Local::now().to_rfc3339();
    let mut accepted = 0usize;
    for segment in &payload.segments {
        if NaiveDate::parse_from_str(&segment.date, "%Y-%m-%d").is_err() {
            continue;
        }
        if segment.segment_type != "work" && segment.segment_type != "away" {
            continue;
        }
        if segment.started_at.is_empty() || segment.ended_at.is_empty() {
            continue;
        }
        conn.execute(
            "
            insert into synced_segments
                (device_id, date, started_at, ended_at, segment_type, received_at)
            values (?1, ?2, ?3, ?4, ?5, ?6)
            on conflict(device_id, date, started_at) do update set
                ended_at = excluded.ended_at,
                segment_type = excluded.segment_type,
                received_at = excluded.received_at
            ",
            params![
                payload.device_id,
                segment.date,
                segment.started_at,
                segment.ended_at,
                segment.segment_type,
                received_at
            ],
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        accepted += 1;
    }
    Ok(Json(serde_json::json!({ "accepted": accepted })))
}

/// 接收客户端设置：按设备幂等 upsert，服务端报表可据此使用统一的工作日历口径。
async fn sync_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SettingsPayload>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let json = serde_json::to_string(&payload.settings)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    conn.execute(
        "
        insert into synced_settings (device_id, settings_json, updated_at)
        values (?1, ?2, ?3)
        on conflict(device_id) do update set
            settings_json = excluded.settings_json,
            updated_at = excluded.updated_at
        ",
        params![payload.device_id, json, Local::now().to_rfc3339()],
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 拉取时段明细：换机后设备 ID 会变，所以返回全部设备的行，
/// 由客户端按日期挑选“当天工作时长最多”的那台，避免把两台机器的时段拼在一起。
async fn pull_segments(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authorize(&headers, &state.token)?;
    let days = query.days.unwrap_or(365).clamp(1, 3660);
    let since = date_n_days_ago(days - 1);
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut stmt = conn
        .prepare(
            "
            select device_id, date, started_at, ended_at, segment_type
            from synced_segments
            where date >= ?1
            order by date, device_id, started_at
            ",
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = stmt
        .query_map(params![since], |row| {
            Ok(PulledSegmentDto {
                device_id: row.get(0)?,
                date: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                segment_type: row.get(4)?,
            })
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let segments: Vec<PulledSegmentDto> = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "segments": segments })))
}

fn query_rows(
    conn: &Connection,
    where_clause: &str,
) -> Result<Vec<(String, Option<String>, Option<String>, i64, i64)>, StatusCode> {
    // 复合主键后同一天可能有多台设备的行：报表/恢复都按“当天活跃最多的那台”
    // 取一条，避免把两台机器的时长重复相加。
    let sql = format!(
        "
        select date, first_activity_at, last_activity_at, active_minutes, idle_minutes
        from synced_records
        where {where_clause}
        order by date desc, active_minutes desc
        "
    );
    let mut stmt = conn.prepare(&sql).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut chosen: Vec<(String, Option<String>, Option<String>, i64, i64)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let row = row.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if seen.insert(row.0.clone()) {
            chosen.push(row);
        }
    }
    Ok(chosen)
}

fn date_n_days_ago(days: i64) -> String {
    (Local::now().date_naive() - chrono::Duration::days(days)).format("%Y-%m-%d").to_string()
}

#[derive(Debug, Deserialize)]
struct RangeQuery {
    days: Option<i64>,
    weeks: Option<i64>,
    months: Option<i64>,
}

async fn daily_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<Json<Vec<ReportRow>>, StatusCode> {
    authorize(&headers, &state.token)?;
    let days = query.days.unwrap_or(30).clamp(1, 366);
    let since = date_n_days_ago(days - 1);
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = query_rows(&conn, &format!("date >= '{since}'"))?;
    Ok(Json(
        rows.into_iter()
            .map(|(date, first, last, active, idle)| ReportRow {
                period: date,
                days: 1,
                active_minutes: active,
                idle_minutes: idle,
                first_activity_at: first,
                last_activity_at: last,
            })
            .collect(),
    ))
}

async fn weekly_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<Json<Vec<ReportRow>>, StatusCode> {
    authorize(&headers, &state.token)?;
    let weeks = query.weeks.unwrap_or(8).clamp(1, 52);
    let since = date_n_days_ago(weeks * 7);
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = query_rows(&conn, &format!("date >= '{since}'"))?;
    Ok(Json(aggregate_by_period(rows, |date| iso_week_key(date))))
}

async fn monthly_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<Json<Vec<ReportRow>>, StatusCode> {
    authorize(&headers, &state.token)?;
    let months = query.months.unwrap_or(12).clamp(1, 36);
    let since = date_n_days_ago(months * 31);
    let conn = state.conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = query_rows(&conn, &format!("date >= '{since}'"))?;
    Ok(Json(aggregate_by_period(rows, |date| date[..7].to_string())))
}

fn iso_week_key(date: &str) -> String {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|parsed| {
            let week = parsed.iso_week().week();
            format!("{}-W{week:02}", parsed.iso_week().year())
        })
        .unwrap_or_else(|_| date.to_string())
}

fn aggregate_by_period(
    rows: Vec<(String, Option<String>, Option<String>, i64, i64)>,
    key_of: impl Fn(&str) -> String,
) -> Vec<ReportRow> {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, ReportRow> = BTreeMap::new();
    for (date, first, last, active, idle) in rows {
        let key = key_of(&date);
        let entry = grouped.entry(key.clone()).or_insert_with(|| ReportRow {
            period: key,
            days: 0,
            active_minutes: 0,
            idle_minutes: 0,
            first_activity_at: None,
            last_activity_at: None,
        });
        entry.days += 1;
        entry.active_minutes += active;
        entry.idle_minutes += idle;
        // 周/月的首次活动取组内最早，最后活动取组内最晚
        entry.first_activity_at = match (&entry.first_activity_at, &first) {
            (_, None) => entry.first_activity_at.clone(),
            (None, Some(value)) => Some(value.clone()),
            (Some(current), Some(value)) => Some(current.min(value).clone()),
        };
        entry.last_activity_at = match (&entry.last_activity_at, &last) {
            (_, None) => entry.last_activity_at.clone(),
            (None, Some(value)) => Some(value.clone()),
            (Some(current), Some(value)) => Some(current.max(value).clone()),
        };
    }
    grouped.into_values().rev().collect()
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Activity Worklog 报表</title>
<style>
  :root { color: #202124; background: #f5f6f4; font-family: Inter, "Segoe UI", "Microsoft YaHei", sans-serif; }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 28px; max-width: 960px; margin-inline: auto; }
  h1 { margin: 0 0 4px; }
  p.sub { color: #68736f; margin: 0 0 20px; }
  .bar { display: flex; gap: 10px; align-items: center; margin-bottom: 20px; flex-wrap: wrap; }
  input { min-height: 40px; padding: 0 12px; border: 1px solid #dfe6e1; border-radius: 8px; background: #fff; font: inherit; flex: 1; }
  button { border: 0; cursor: pointer; min-height: 40px; padding: 0 16px; border-radius: 8px; color: #fff; background: #2c7d57; font: inherit; }
  section { border: 1px solid #dfe6e1; border-radius: 8px; background: #fff; margin-bottom: 18px; overflow: hidden; }
  section h2 { margin: 0; padding: 14px 18px; border-bottom: 1px solid #e6ece8; font-size: 1.05rem; }
  table { width: 100%; border-collapse: collapse; font-size: 0.92rem; }
  th, td { text-align: left; padding: 10px 18px; border-bottom: 1px solid #eef2ef; }
  th { color: #68736f; font-weight: 500; background: #f8faf8; }
  .msg { color: #b3261e; }
</style>
</head>
<body>
<h1>Activity Worklog 报表</h1>
<p class="sub">日报 / 周报 / 月报（数据来自客户端同步上传）</p>
<div class="bar">
  <input id="token" type="password" placeholder="访问 Token（见服务器控制台输出）" />
  <button onclick="load()">刷新</button>
</div>
<p id="msg" class="msg"></p>
<section><h2>日报（近 14 天）</h2><table id="daily"></table></section>
<section><h2>周报（近 8 周）</h2><table id="weekly"></table></section>
<section><h2>月报（近 12 个月）</h2><table id="monthly"></table></section>
<script>
function fmt(mins) {
  const h = Math.floor(mins / 60), m = mins % 60;
  return h + "小时" + String(m).padStart(2, "0") + "分";
}
async function fetchReport(path) {
  const token = document.getElementById("token").value.trim();
  const res = await fetch(path, { headers: { "Authorization": "Bearer " + token } });
  if (!res.ok) throw new Error("HTTP " + res.status);
  return res.json();
}
function render(tableId, rows) {
  const head = "<tr><th>周期</th><th>天数</th><th>活跃时长</th><th>空闲时长</th><th>首次活动</th><th>最后活动</th></tr>";
  const body = rows.map(r =>
    "<tr><td>" + r.period + "</td><td>" + r.days + "</td><td>" + fmt(r.active_minutes) +
    "</td><td>" + fmt(r.idle_minutes) + "</td><td>" + (r.first_activity_at ?? "--:--") +
    "</td><td>" + (r.last_activity_at ?? "--:--") + "</td></tr>"
  ).join("");
  document.getElementById(tableId).innerHTML = head + (body || "<tr><td colspan='6'>暂无数据</td></tr>");
}
async function load() {
  const msg = document.getElementById("msg");
  msg.textContent = "";
  try {
    render("daily", await fetchReport("/api/reports/daily?days=14"));
    render("weekly", await fetchReport("/api/reports/weekly?weeks=8"));
    render("monthly", await fetchReport("/api/reports/monthly?months=12"));
  } catch (err) {
    msg.textContent = "加载失败：" + err.message + "（请确认 Token 是否正确）";
  }
}
</script>
</body>
</html>
"#;

#[tokio::main]
async fn main() {
    let conn = open_db().expect("打开数据库失败");
    let token = load_or_create_token().expect("生成 Token 失败");

    // 默认只监听本机，保证开箱即用的安全；需要跨机器同步时，
    // 用环境变量 WORKBLOG_BIND 指定监听地址（如 0.0.0.0:8787），
    // 并务必在前面挂带 TLS 的反向代理，本服务不内置 HTTPS。
    let bind_raw =
        std::env::var("WORKBLOG_BIND").unwrap_or_else(|_| "127.0.0.1:8787".to_string());
    let bind: SocketAddr = bind_raw
        .parse()
        .unwrap_or_else(|err| panic!("无法解析 WORKBLOG_BIND={bind_raw}: {err}"));
    if bind.ip().is_unspecified() || !bind.ip().is_loopback() {
        println!(
            "警告：服务监听在非回环地址 {bind}，请确认已通过反向代理提供 HTTPS，且网络访问受控。"
        );
    }
    println!("Activity Worklog 报表服务已启动：http://{bind}");
    println!("访问 Token：{token}");
    println!("（Token 保存在 {}，客户端“数据同步”页需填入同一 Token）", data_dir().join("server-token.txt").display());

    // 服务只监听 127.0.0.1，本就不可被外部网络访问；CORS 进一步收紧到本机来源，
    // 避免同机其他来源的前端跨域调用同步/报表接口（不再 permissive）。
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            let bytes = origin.as_bytes();
            bytes.starts_with(b"http://127.0.0.1") || bytes.starts_with(b"http://localhost")
        }))
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/health", get(health))
        .route("/api/sync/records", post(sync_records).get(pull_records))
        .route("/api/sync/segments", post(sync_segments).get(pull_segments))
        .route("/api/sync/settings", post(sync_settings))
        .route("/api/sync/delete", post(delete_records))
        .route("/api/reports/daily", get(daily_report))
        .route("/api/reports/weekly", get(weekly_report))
        .route("/api/reports/monthly", get(monthly_report))
        .layer(cors)
        .with_state(std::sync::Arc::new(AppState {
            conn: Mutex::new(conn),
            token,
        }));

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|err| panic!("绑定 {bind} 失败：{err}"));
    axum::serve(listener, app).await.expect("服务启动失败");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_uses_system_csprng() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second, "两次生成的 Token 不能相同");
    }

    #[test]
    fn authorize_only_accepts_matching_token() {
        let mut headers = HeaderMap::new();
        assert!(authorize(&headers, "secret").is_err());
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        assert!(authorize(&headers, "secret").is_ok());
        assert!(authorize(&headers, "other").is_err());
        headers.insert("authorization", HeaderValue::from_static("Bearer "));
        assert!(authorize(&headers, "secret").is_err());
    }

    #[test]
    fn iso_week_uses_iso_calendar_year() {
        assert_eq!(iso_week_key("2025-12-29"), "2026-W01");
        assert_eq!(iso_week_key("2026-01-01"), "2026-W01");
        assert_eq!(iso_week_key("not-a-date"), "not-a-date");
    }

    #[test]
    fn aggregate_groups_rows_by_period() {
        let rows: Vec<(String, Option<String>, Option<String>, i64, i64)> = vec![
            (
                "2026-01-05".to_string(),
                Some("09:00".to_string()),
                Some("18:00".to_string()),
                100,
                10,
            ),
            (
                "2026-01-06".to_string(),
                Some("08:30".to_string()),
                Some("17:00".to_string()),
                200,
                20,
            ),
        ];
        let result = aggregate_by_period(rows, |date| date[..7].to_string());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].period, "2026-01");
        assert_eq!(result[0].days, 2);
        assert_eq!(result[0].active_minutes, 300);
        assert_eq!(result[0].idle_minutes, 30);
        assert_eq!(result[0].first_activity_at.as_deref(), Some("08:30"));
        assert_eq!(result[0].last_activity_at.as_deref(), Some("18:00"));
    }
}
