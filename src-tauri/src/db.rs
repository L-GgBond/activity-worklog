use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, Timelike};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{from_str, to_string_pretty};

use crate::settings::AppSettings;

/// 两次采样间隔只有 15 秒；超过这个缺口说明进程被休眠/关闭过，
/// 不能再把上一段的结束时间直接延长到当前时刻（否则整段空档会被算成工作）。
const GAP_RESET_SECONDS: i64 = 150;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRecord {
    pub date: String,
    pub first_activity_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub active_minutes: i64,
    pub idle_minutes: i64,
    pub sync_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivitySegment {
    pub date: String,
    pub started_at: String,
    pub ended_at: String,
    pub segment_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncSummary {
    pub pending: i64,
    pub synced: i64,
    pub last_synced_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HourBucket {
    pub hour: u32,
    pub minutes: i64,
}

pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
}

/// WAL + NORMAL 同步 + 忙等待：后台 15 秒一写时显著减少 fsync 开销，
/// 也避免极端情况下 UI 读与采集写互相 SQLITE_BUSY。
/// 这些是「连接级」设置，不跟随数据库文件；恢复备份重开连接后必须重新执行一遍。
fn configure_connection(conn: &Connection) -> anyhow::Result<()> {
    let _ = conn.query_row("pragma journal_mode = wal", [], |row| {
        row.get::<_, String>(0)
    });
    conn.execute_batch("pragma synchronous = normal; pragma busy_timeout = 5000;")?;
    Ok(())
}

/// SQLite 的 -wal / -shm 伴随文件路径。用 OsString 拼接，
/// 避免数据目录含非 ASCII 字符时 to_string_lossy() 把路径改写掉。
fn wal_sidecar(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    db_path.with_file_name(name)
}

impl Store {
    pub fn open() -> anyhow::Result<Self> {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("activity-worklog");
        fs::create_dir_all(&data_dir)?;
        let conn = Connection::open(data_dir.join("worklog.sqlite3"))?;
        configure_connection(&conn)?;
        let store = Self { conn, data_dir };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "
            create table if not exists daily_work_records (
                date text primary key,
                first_activity_at text,
                last_activity_at text,
                active_minutes integer not null default 0,
                idle_minutes integer not null default 0,
                sync_status text not null default 'pending',
                updated_at text not null
            );

            create table if not exists settings (
                key text primary key,
                value text not null
            );

            create table if not exists activity_segments (
                id integer primary key autoincrement,
                date text not null,
                started_at text not null,
                ended_at text not null,
                segment_type text not null check(segment_type in ('work', 'away'))
            );

            create index if not exists idx_activity_segments_date on activity_segments(date);
            ",
        )?;
        Ok(())
    }

    /// 记录一次活动采样，并维护工作/离开段落。
    ///
    /// 模型：`segments` 缓冲当天全部段落，其中"当天最后一段"即当前进行中的段落，
    /// 每次采样若类型不变就延长其结束时间，类型变化则关闭旧段、开启新段。
    ///
    /// - `in_work_session`: 本次采样是否仍处于工作会话（空闲未超过离开阈值）。
    /// - `backdate`: 开启新段落时，把起点回溯到最后一次真实输入的时刻。
    pub fn record_activity(
        &mut self,
        timestamp: DateTime<Local>,
        segments: &mut Vec<ActivitySegment>,
        in_work_session: bool,
        backdate: Option<Duration>,
    ) -> anyhow::Result<bool> {
        let date = timestamp.format("%Y-%m-%d").to_string();
        let time = timestamp.format("%H:%M:%S").to_string();
        let desired_type = if in_work_session { "work" } else { "away" };

        // 往日残留段落一律封闭，并纳入需要重算的日期集合
        let mut affected_dates: BTreeSet<String> = BTreeSet::new();
        affected_dates.insert(date.clone());
        for segment in segments.iter_mut() {
            if segment.date != date {
                if segment.ended_at.is_empty() {
                    segment.ended_at = "23:59:59".to_string();
                }
                affected_dates.insert(segment.date.clone());
            }
        }

        // 当前段落 = 当天最后一段
        let current_idx = segments.iter().rposition(|segment| segment.date == date);

        // 上一段的结束时间与当前时间存在大缺口（休眠 / 进程被挂起）时，
        // 不延长旧段，直接在缺口之后开新段。
        let gap_break = match current_idx {
            Some(idx) => {
                let last_end = if segments[idx].ended_at.is_empty() {
                    segments[idx].started_at.clone()
                } else {
                    segments[idx].ended_at.clone()
                };
                parse_time_seconds(&time) - parse_time_seconds(&last_end) > GAP_RESET_SECONDS
            }
            None => false,
        };

        match current_idx {
            Some(idx) if gap_break => {
                let floor = segments[idx].started_at.clone();
                let new_started_at =
                    self.new_segment_start(&time, &floor, backdate, timestamp, &date);
                segments.push(ActivitySegment {
                    date: date.clone(),
                    started_at: new_started_at,
                    ended_at: time,
                    segment_type: desired_type.to_string(),
                });
            }
            Some(idx) if segments[idx].segment_type == desired_type => {
                // 类型不变，仅延长结束时间
                segments[idx].ended_at = time;
            }
            Some(idx) => {
                // 类型切换：新段起点回溯到最后输入时刻，旧段结束于新段起点
                let floor = segments[idx].started_at.clone();
                let new_started_at =
                    self.new_segment_start(&time, &floor, backdate, timestamp, &date);
                segments[idx].ended_at = new_started_at.clone();
                segments.push(ActivitySegment {
                    date: date.clone(),
                    started_at: new_started_at,
                    ended_at: time,
                    segment_type: desired_type.to_string(),
                });
            }
            None => {
                // 当天首段：起点回溯到最后输入时刻，但不早于零点
                let new_started_at =
                    self.new_segment_start(&time, "00:00:00", backdate, timestamp, &date);
                segments.push(ActivitySegment {
                    date: date.clone(),
                    started_at: new_started_at,
                    ended_at: time,
                    segment_type: desired_type.to_string(),
                });
            }
        }

        // 全量持久化受影响日期的段落，并基于段落重算统计；
        // 统计没有变化时跳过写库、不置 pending（返回是否有实际写入）
        let mut changed = false;
        for affected in &affected_dates {
            self.persist_day_segments(affected, segments)?;
            let stats = segment_stats(segments, affected);
            let updated_at = timestamp.to_rfc3339();
            if self.upsert_daily_stats(affected, &stats, &updated_at)? {
                changed = true;
            }
        }
        Ok(changed)
    }

    /// 从服务器补回本地缺失的日期：只插入不存在的日期，绝不覆盖本地已有记录
    /// （本地可能已经手工编辑过时段，覆盖会丢掉用户的修改）。
    /// 补回的记录直接标记为 synced，不会因为“新插入”又被推回服务器。
    pub fn import_missing_records(&self, records: &[DailyRecord]) -> anyhow::Result<usize> {
        let updated_at = Local::now().to_rfc3339();
        let mut inserted = 0usize;
        for record in records {
            if NaiveDate::parse_from_str(&record.date, "%Y-%m-%d").is_err() {
                continue;
            }
            let changed = self.conn.execute(
                "
                insert into daily_work_records
                    (date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status, updated_at)
                values (?1, ?2, ?3, ?4, ?5, 'synced', ?6)
                on conflict(date) do nothing
                ",
                params![
                    record.date,
                    record.first_activity_at,
                    record.last_activity_at,
                    record.active_minutes,
                    record.idle_minutes,
                    &updated_at
                ],
            )?;
            inserted += changed;
        }
        Ok(inserted)
    }

    /// 从服务器补回时段明细：只补本地完全没有时段的日期，
    /// 与日汇总的“只补缺失”口径一致，绝不覆盖手工编辑过的日子。
    pub fn import_missing_segments(&self, incoming: &[ActivitySegment]) -> anyhow::Result<usize> {
        let mut existing_dates: BTreeSet<String> = BTreeSet::new();
        {
            let mut stmt = self.conn.prepare("select distinct date from activity_segments")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                existing_dates.insert(row?);
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        for segment in incoming {
            if existing_dates.contains(&segment.date) {
                continue;
            }
            tx.execute(
                "insert into activity_segments (date, started_at, ended_at, segment_type)
                 values (?1, ?2, ?3, ?4)",
                params![
                    segment.date,
                    segment.started_at,
                    segment.ended_at,
                    segment.segment_type
                ],
            )?;
            inserted += 1;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 写入某天的日统计，返回是否有实际写入：
    /// 数据没变时跳过，避免「已同步的天被无意义的采样周期反复打回待同步」。
    fn upsert_daily_stats(
        &self,
        date: &str,
        stats: &DayStats,
        updated_at: &str,
    ) -> anyhow::Result<bool> {
        let existing: Option<(Option<String>, Option<String>, i64, i64)> = self
            .conn
            .query_row(
                "select first_activity_at, last_activity_at, active_minutes, idle_minutes
                 from daily_work_records where date = ?1",
                params![date],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();

        if let Some((first, last, active, idle)) = &existing {
            // 与下方 upsert 的 coalesce 语义保持一致：
            // 首次活动沿用旧值，最后活动优先用新值
            let merged_first = first.clone().or_else(|| stats.first_activity_at.clone());
            let merged_last = stats
                .last_activity_at
                .clone()
                .or_else(|| last.clone());
            if merged_first == *first
                && merged_last == *last
                && stats.active_minutes == *active
                && stats.idle_minutes == *idle
            {
                return Ok(false);
            }
        }

        self.conn.execute(
            "
            insert into daily_work_records
                (date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status, updated_at)
            values
                (?1, ?2, ?3, ?5, ?6, 'pending', ?4)
            on conflict(date) do update set
                first_activity_at = coalesce(daily_work_records.first_activity_at, excluded.first_activity_at),
                last_activity_at = coalesce(excluded.last_activity_at, daily_work_records.last_activity_at),
                active_minutes = excluded.active_minutes,
                idle_minutes = excluded.idle_minutes,
                sync_status = 'pending',
                updated_at = excluded.updated_at
            ",
            params![
                date,
                &stats.first_activity_at,
                &stats.last_activity_at,
                updated_at,
                stats.active_minutes,
                stats.idle_minutes
            ],
        )?;
        Ok(true)
    }

    /// 计算新段落的起点时间：可选回溯到最后输入时刻，但不早于下限（避免段落倒挂）。
    fn new_segment_start(
        &self,
        time: &str,
        floor: &str,
        backdate: Option<Duration>,
        timestamp: DateTime<Local>,
        date: &str,
    ) -> String {
        let mut started_at = time.to_string();

        if let Some(backdate) = backdate {
            if let Ok(span) = chrono::Duration::from_std(backdate) {
                let backdated = timestamp - span;
                if backdated.format("%Y-%m-%d").to_string() == date {
                    started_at = backdated.format("%H:%M:%S").to_string();
                }
            }
        }

        if parse_time_seconds(&started_at) < parse_time_seconds(floor) {
            started_at = floor.to_string();
        }

        started_at
    }

    fn persist_day_segments(
        &self,
        date: &str,
        segments: &[ActivitySegment],
    ) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("delete from activity_segments where date = ?1", params![date])?;
        for segment in segments.iter().filter(|segment| segment.date == date) {
            tx.execute(
                "
                insert into activity_segments (date, started_at, ended_at, segment_type)
                values (?1, ?2, ?3, ?4)
                ",
                params![segment.date, segment.started_at, segment.ended_at, segment.segment_type],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 启动时加载当天段落，恢复段落缓冲。
    pub fn load_segments_for_today(&self) -> anyhow::Result<Vec<ActivitySegment>> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut loaded = self.list_segments_by_date(&today)?;
        // 兼容旧数据：结束时间缺失时补为起点
        for segment in loaded.iter_mut() {
            if segment.ended_at.is_empty() {
                segment.ended_at = segment.started_at.clone();
            }
        }
        Ok(loaded)
    }

    /// 暂停记录或退出时调用：把当天段落的结束时间截断到关闭时刻并落库。
    pub fn close_open_segments(
        &mut self,
        segments: &mut Vec<ActivitySegment>,
        timestamp: DateTime<Local>,
    ) -> anyhow::Result<()> {
        let date = timestamp.format("%Y-%m-%d").to_string();
        let time = timestamp.format("%H:%M:%S").to_string();
        for segment in segments.iter_mut() {
            if segment.date != date {
                continue;
            }
            if segment.ended_at.is_empty() || parse_time_seconds(&segment.ended_at) > parse_time_seconds(&time) {
                segment.ended_at = time.clone();
            }
        }
        self.persist_day_segments(&date, segments)?;
        let stats = segment_stats(segments, &date);
        let updated_at = timestamp.to_rfc3339();
        let _ = self.upsert_daily_stats(&date, &stats, &updated_at)?;
        Ok(())
    }

    pub fn list_segments_by_date(&self, date: &str) -> anyhow::Result<Vec<ActivitySegment>> {
        let mut stmt = self.conn.prepare(
            "
            select date, started_at, ended_at, segment_type
            from activity_segments
            where date = ?1
            order by id
            ",
        )?;
        let rows = stmt.query_map(params![date], |row| {
            Ok(ActivitySegment {
                date: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                segment_type: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 全量时段明细（JSON 导出用），不受历史列表窗口限制
    pub fn list_all_segments(&self) -> anyhow::Result<Vec<ActivitySegment>> {
        let mut stmt = self.conn.prepare(
            "
            select date, started_at, ended_at, segment_type
            from activity_segments
            order by date, id
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActivitySegment {
                date: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                segment_type: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 历史列表：按日期倒序取窗口内的记录（limit 由前端控制，可「加载更早」）
    pub fn list_daily_records(&self, limit: u32) -> anyhow::Result<Vec<DailyRecord>> {
        let limit = limit.clamp(1, 5000);
        let mut stmt = self.conn.prepare(
            "
            select date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status
            from daily_work_records
            order by date desc
            limit ?1
            ",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(DailyRecord {
                date: row.get(0)?,
                first_activity_at: row.get(1)?,
                last_activity_at: row.get(2)?,
                active_minutes: row.get(3)?,
                idle_minutes: row.get(4)?,
                sync_status: row.get(5)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 全量列表（CSV 导出用），不受历史列表窗口限制
    pub fn list_all_daily_records(&self) -> anyhow::Result<Vec<DailyRecord>> {
        let mut stmt = self.conn.prepare(
            "
            select date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status
            from daily_work_records
            order by date desc
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DailyRecord {
                date: row.get(0)?,
                first_activity_at: row.get(1)?,
                last_activity_at: row.get(2)?,
                active_minutes: row.get(3)?,
                idle_minutes: row.get(4)?,
                sync_status: row.get(5)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 历史总天数
    pub fn count_daily_records(&self) -> anyhow::Result<i64> {
        self.conn
            .query_row(
                "select count(1) from daily_work_records",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn pending_records(&self) -> anyhow::Result<Vec<DailyRecord>> {
        let mut stmt = self.conn.prepare(
            "
            select date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status
            from daily_work_records
            where sync_status = 'pending'
            order by date desc
            limit 500
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DailyRecord {
                date: row.get(0)?,
                first_activity_at: row.get(1)?,
                last_activity_at: row.get(2)?,
                active_minutes: row.get(3)?,
                idle_minutes: row.get(4)?,
                sync_status: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn mark_synced(&self, dates: &[String]) -> anyhow::Result<()> {
        if dates.is_empty() {
            return Ok(());
        }
        let updated_at = Local::now().to_rfc3339();
        for date in dates {
            self.conn.execute(
                "update daily_work_records set sync_status = 'synced', updated_at = ?2 where date = ?1",
                params![date, &updated_at],
            )?;
        }
        Ok(())
    }

    pub fn get_settings(&self) -> anyhow::Result<AppSettings> {
        let value: Option<String> = self
            .conn
            .query_row("select value from settings where key = 'app'", [], |row| row.get(0))
            .ok();
        Ok(value
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &AppSettings) -> anyhow::Result<()> {
        self.conn.execute(
            "
            insert into settings (key, value) values ('app', ?1)
            on conflict(key) do update set value = excluded.value
            ",
            params![serde_json::to_string(settings)?],
        )?;
        Ok(())
    }

    pub fn export_csv(&self) -> anyhow::Result<PathBuf> {
        let dir = dirs::download_dir()
            .map(|base| base.join("Activity Worklog"))
            .unwrap_or_else(|| self.data_dir.clone());
        fs::create_dir_all(&dir)?;
        // 带时间戳，避免静默覆盖上一次导出；走全量查询，不受历史列表窗口限制
        let path = dir.join(format!(
            "daily-work-records-{}.csv",
            Local::now().format("%Y%m%d-%H%M%S")
        ));
        let mut writer = csv::Writer::from_path(&path)?;
        for record in self.list_all_daily_records()? {
            writer.serialize(record)?;
        }
        writer.flush()?;
        Ok(path)
    }

    /// JSON 全量导出格式：同时包含日汇总与逐时段明细，便于跨机迁移与人工校订。
    /// 相比 CSV（仅日汇总）补全了时段级数据，相比 SQLite 热备（二进制）是纯文本、可读可改。
    #[derive(Serialize, Deserialize)]
    struct JsonExport {
        version: u32,
        exported_at: String,
        daily_work_records: Vec<DailyRecord>,
        activity_segments: Vec<ActivitySegment>,
        // 旧文件没有这一项，缺省即可，不影响历史备份的读取
        #[serde(default)]
        settings: Option<AppSettings>,
    }

    /// 导出全量数据为 JSON 文件（写入下载目录 / Activity Worklog），返回文件路径。
    pub fn export_json(&self) -> anyhow::Result<PathBuf> {
        let dir = dirs::download_dir()
            .map(|base| base.join("Activity Worklog"))
            .unwrap_or_else(|| self.data_dir.clone());
        fs::create_dir_all(&dir)?;
        // 带时间戳，避免静默覆盖上一次导出
        let path = dir.join(format!(
            "worklog-export-{}.json",
            Local::now().format("%Y%m%d-%H%M%S")
        ));
        let payload = JsonExport {
            version: 1,
            exported_at: Local::now().to_rfc3339(),
            daily_work_records: self.list_all_daily_records()?,
            activity_segments: self.list_all_segments()?,
            settings: Some(self.get_settings()?),
        };
        let text = to_string_pretty(&payload)
            .map_err(|err| format!("序列化 JSON 失败: {err}"))?;
        fs::write(&path, text)?;
        Ok(path)
    }

    /// 从 JSON 文本导入全量数据：在单个事务内清空两表后重写，保证一致性。
    /// 留空/无效字段由列约束（segment_type 等）兜底，事务整体回滚并上报错误。
    /// 返回导入的天数。sync_status 原样保留（ faithful round-trip），导入后按本地需要自行决定是否重新同步。
    pub fn import_json(&self, content: &str) -> anyhow::Result<u32> {
        let payload: JsonExport = from_str(content)
            .map_err(|err| format!("解析 JSON 失败: {err}"))?;
        if payload.version != 1 {
            anyhow::bail!("不支持的导出版本: {}", payload.version);
        }
        let now = Local::now().to_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute("delete from activity_segments", [])?;
        tx.execute("delete from daily_work_records", [])?;
        for rec in &payload.daily_work_records {
            tx.execute(
                "insert into daily_work_records
                 (date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.date,
                    rec.first_activity_at,
                    rec.last_activity_at,
                    rec.active_minutes,
                    rec.idle_minutes,
                    rec.sync_status,
                    now
                ],
            )?;
        }
        for seg in &payload.activity_segments {
            tx.execute(
                "insert into activity_segments (date, started_at, ended_at, segment_type)
                 values (?1, ?2, ?3, ?4)",
                params![seg.date, seg.started_at, seg.ended_at, seg.segment_type],
            )?;
        }
        tx.commit()?;
        // 备份里带了设置就一并恢复（含同步地址、节假日、作息等），
        // 密码哈希也在其中，恢复后应用锁状态自然生效。
        if let Some(settings) = payload.settings {
            self.save_settings(&settings)?;
        }
        Ok(payload.daily_work_records.len() as u32)
    }

    /// 数据库文件路径（用于备份/恢复）。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("worklog.sqlite3")
    }

    /// 删除某一天的全部记录（日汇总 + 时段），用于纠正误记。
    pub fn delete_day(&self, date: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "delete from activity_segments where date = ?1",
            params![date],
        )?;
        self.conn.execute(
            "delete from daily_work_records where date = ?1",
            params![date],
        )?;
        Ok(())
    }

    /// 清空全部活动记录（保留设置），返回删除的天数。
    pub fn clear_records(&self) -> anyhow::Result<i64> {
        self.conn.execute("delete from activity_segments", [])?;
        let removed = self.conn.execute("delete from daily_work_records", [])?;
        Ok(removed as i64)
    }

    /// 用前端传回的整段列表覆盖某天的时段，并重算当日统计（置 pending）。
    /// 同时做基础校验：日期一致、起止非空、开始不晚于结束、类型合法。
    /// 让用户手动修正误记的时段，而不必整日删除。
    pub fn replace_day_segments(&self, date: &str, segments: &[ActivitySegment]) -> anyhow::Result<()> {
        for seg in segments {
            if seg.date != date {
                anyhow::bail!("时段日期与请求日期不一致");
            }
            if seg.started_at.is_empty() || seg.ended_at.is_empty() {
                anyhow::bail!("时段的起止时间不能为空");
            }
            if seg.segment_type != "work" && seg.segment_type != "away" {
                anyhow::bail!("时段类型无效，只能是 work 或 away");
            }
            if parse_time_seconds(&seg.started_at) > parse_time_seconds(&seg.ended_at) {
                anyhow::bail!("开始时间不能晚于结束时间");
            }
        }
        // 相邻时段不得重叠：重叠会让 segment_stats 把同一段时间重复累计，
        // 手工编辑/补录时最容易犯这个错，必须在落库前拦住。
        let mut ranges: Vec<(i64, i64)> = segments
            .iter()
            .map(|seg| {
                (
                    parse_time_seconds(&seg.started_at),
                    parse_time_seconds(&seg.ended_at),
                )
            })
            .collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            if pair[1].0 < pair[0].1 {
                anyhow::bail!("时段之间存在重叠，请调整后再保存");
            }
        }
        self.persist_day_segments(date, segments)?;
        let stats = segment_stats(segments, date);
        let updated_at = Local::now().to_rfc3339();
        self.upsert_daily_stats(date, &stats, &updated_at)?;
        Ok(())
    }

    /// 数据保留期清理：删除早于 cutoff（不含）的日汇总与对应时段。
    /// cutoff 格式 "YYYY-MM-DD"，返回被删除的天数。
    pub fn purge_old_records(&self, cutoff: &str) -> anyhow::Result<i64> {
        self.conn
            .execute("delete from activity_segments where date < ?1", params![cutoff])?;
        let removed = self.conn.execute(
            "delete from daily_work_records where date < ?1",
            params![cutoff],
        )?;
        Ok(removed as i64)
    }

    /// 同步概况：待同步天数 / 已同步天数 / 上次同步时间（落库时间）。
    pub fn sync_summary(&self) -> anyhow::Result<SyncSummary> {
        let pending: i64 = self.conn.query_row(
            "select count(1) from daily_work_records where sync_status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        let synced: i64 = self.conn.query_row(
            "select count(1) from daily_work_records where sync_status = 'synced'",
            [],
            |row| row.get(0),
        )?;
        let last_synced_at: Option<String> = self.conn.query_row(
            "select max(updated_at) from daily_work_records where sync_status = 'synced'",
            [],
            |row| row.get(0),
        )?;
        Ok(SyncSummary {
            pending,
            synced,
            last_synced_at,
        })
    }

    /// 列出自动备份目录里的备份文件（名称、字节数、修改时间），供数据健康面板展示。
    pub fn list_backups(&self) -> Vec<(String, u64, String)> {
        let dir = self.data_dir.join("backups");
        let mut files: Vec<(String, u64, std::time::SystemTime)> = match fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let meta = entry.metadata().ok()?;
                    if meta.is_file() {
                        Some((entry.file_name().to_string_lossy().to_string(), meta.len(), meta.modified().ok()?))
                    } else {
                        None
                    }
                })
                .collect(),
            Err(_) => return Vec::new(),
        };
        files.sort_by(|a, b| b.2.cmp(&a.2));
        files
            .into_iter()
            .map(|(name, size, modified)| {
                let modified = DateTime::<Local>::from(modified).format("%Y-%m-%d %H:%M").to_string();
                (name, size, modified)
            })
            .collect()
    }

    /// 用 SQLite 的 VACUUM INTO 生成一致性备份（热备份，不阻塞当前连接）。
    pub fn backup_to(&self, dest: &Path) -> anyhow::Result<()> {
        if dest.exists() {
            fs::remove_file(dest)?;
        }
        self.conn.execute(
            "vacuum into ?1",
            params![dest.to_string_lossy().to_string()],
        )?;
        Ok(())
    }

    /// 自动备份：写入数据目录下的 backups/，只保留最近 keep 份。
    /// 由后台循环按天调用，用户不需要记得手动点“备份”。
    pub fn auto_backup(&self, keep: usize) -> anyhow::Result<PathBuf> {
        let dir = self.data_dir.join("backups");
        fs::create_dir_all(&dir)?;
        let dest = dir.join(format!(
            "auto-backup-{}.sqlite3",
            Local::now().format("%Y%m%d-%H%M%S")
        ));
        self.backup_to(&dest)?;

        let mut files: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().map(|value| value == "sqlite3").unwrap_or(false)
                    && path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(|value| value.starts_with("auto-backup-"))
                        .unwrap_or(false)
            })
            .collect();
        // 文件名带时间戳，按名称倒序即按时间倒序
        files.sort();
        if files.len() > keep {
            for stale in files.iter().take(files.len() - keep) {
                let _ = fs::remove_file(stale);
            }
        }
        Ok(dest)
    }

    /// 从自动备份目录中按文件名恢复（供数据健康面板直接选择）。
    /// 文件名做白名单校验，杜绝路径穿越。
    pub fn restore_named_backup(&mut self, name: &str) -> anyhow::Result<()> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.contains('\0')
        {
            anyhow::bail!("非法备份文件名");
        }
        let source = self.data_dir.join("backups").join(name);
        self.restore_from(&source)
    }

    /// 用备份文件替换当前数据库：校验 → 归一化快照 → 断开旧句柄 → 清理 WAL → 覆盖 → 重开。
    ///
    /// 两个必须处理的坑：
    /// 1. 直接覆盖运行中的库文件，旧句柄后续写入会落进已被替换的旧 inode；
    /// 2. 开启 WAL 后强杀进程会残留 -wal / -shm，只覆盖主库的话新连接会 replay
    ///    旧事务，把刚恢复的数据又改回去。
    pub fn restore_from(&mut self, source: &Path) -> anyhow::Result<()> {
        if !source.exists() {
            anyhow::bail!("备份文件不存在");
        }
        let target = self.db_path();
        let staged = self.data_dir.join("worklog.restore.tmp");
        {
            // 校验表结构，同时用 VACUUM INTO 生成不含 WAL 的干净快照。
            // 若备份是直接拷贝的主库且旁边带着 -wal，这一步会把 WAL 里的事务一并固化进快照。
            let probe = Connection::open(source)?;
            probe.query_row("select count(1) from daily_work_records", [], |row| {
                row.get::<_, i64>(0)
            })?;
            if staged.exists() {
                fs::remove_file(&staged)?;
            }
            probe.execute(
                "vacuum into ?1",
                params![staged.to_string_lossy().to_string()],
            )?;
        }
        // probe 已释放；先换成内存库释放主库句柄（正常关闭时 SQLite 会自行 checkpoint）
        self.conn = Connection::open_in_memory()?;
        // 无论上次是正常退出还是强杀，都清掉残留伴随文件再覆盖
        let _ = fs::remove_file(wal_sidecar(&target, "-wal"));
        let _ = fs::remove_file(wal_sidecar(&target, "-shm"));
        fs::copy(&staged, &target)?;
        let _ = fs::remove_file(&staged);
        self.conn = Connection::open(&target)?;
        configure_connection(&self.conn)?;
        self.migrate()?;
        Ok(())
    }

    /// 按小时聚合最近 range_days 天的工作（活跃）时长，返回 0..24 每个小时的分钟数。
    pub fn hourly_activity(&self, range_days: u32) -> anyhow::Result<Vec<HourBucket>> {
        let since = (Local::now() - chrono::Duration::days(range_days.saturating_sub(1) as i64))
            .format("%Y-%m-%d")
            .to_string();
        let mut stmt = self.conn.prepare(
            "select started_at, ended_at from activity_segments where segment_type = 'work' and date >= ?1",
        )?;
        let rows = stmt.query_map(params![since], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut buckets = [0i64; 24];
        for row in rows {
            let (s, e) = row?;
            let ss = parse_time_seconds(&s);
            let ee = if e.is_empty() { ss } else { parse_time_seconds(&e) };
            let (mut cur, ee) = (ss.min(ee), ss.max(ee));
            while cur < ee {
                let hour = (cur / 3600) as usize;
                let end_of_hour = ((hour as i64 + 1) * 3600).min(ee);
                buckets[hour] += end_of_hour - cur;
                cur = end_of_hour;
            }
        }

        Ok((0..24)
            .map(|h| HourBucket {
                hour: h,
                minutes: buckets[h as usize] / 60,
            })
            .collect())
    }
}

pub struct DayStats {
    pub first_activity_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub active_minutes: i64,
    pub idle_minutes: i64,
}

/// 基于段落数据重算某天的统计数据（按秒累计，彻底修复分钟数恒为 0 的问题）。
pub fn segment_stats(segments: &[ActivitySegment], date: &str) -> DayStats {
    let mut first_activity_at: Option<String> = None;
    let mut last_activity_at: Option<String> = None;
    let mut active_seconds: i64 = 0;
    let mut away_seconds: i64 = 0;

    for segment in segments.iter().filter(|segment| segment.date == date) {
        let started = parse_time_seconds(&segment.started_at);
        let ended = if segment.ended_at.is_empty() {
            started
        } else {
            parse_time_seconds(&segment.ended_at)
        };
        let duration = (ended - started).max(0);

        if segment.segment_type == "work" {
            if first_activity_at.is_none() {
                first_activity_at = Some(segment.started_at.clone());
            }
            let last = if segment.ended_at.is_empty() {
                &segment.started_at
            } else {
                &segment.ended_at
            };
            last_activity_at = Some(last.clone());
            active_seconds += duration;
        } else {
            away_seconds += duration;
        }
    }

    DayStats {
        first_activity_at,
        last_activity_at,
        active_minutes: active_seconds / 60,
        idle_minutes: away_seconds / 60,
    }
}

fn parse_time_seconds(time: &str) -> i64 {
    chrono::NaiveTime::parse_from_str(time, "%H:%M:%S")
        .map(|parsed| {
            parsed.hour() as i64 * 3600 + parsed.minute() as i64 * 60 + parsed.second() as i64
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(date: &str, start: &str, end: &str, kind: &str) -> ActivitySegment {
        ActivitySegment {
            date: date.to_string(),
            started_at: start.to_string(),
            ended_at: end.to_string(),
            segment_type: kind.to_string(),
        }
    }

    #[test]
    fn segment_stats_only_counts_requested_date() {
        let segments = vec![
            segment("2026-01-05", "09:00:00", "10:30:00", "work"),
            segment("2026-01-05", "10:30:00", "10:45:00", "away"),
            segment("2026-01-06", "09:00:00", "12:00:00", "work"),
        ];
        let stats = segment_stats(&segments, "2026-01-05");
        assert_eq!(stats.active_minutes, 90);
        assert_eq!(stats.idle_minutes, 15);
        assert_eq!(stats.first_activity_at.as_deref(), Some("09:00:00"));
        assert_eq!(stats.last_activity_at.as_deref(), Some("10:30:00"));
    }

    #[test]
    fn segment_stats_handles_open_and_reversed_segments() {
        let segments = vec![
            segment("2026-01-05", "09:00:00", "", "work"),
            segment("2026-01-05", "11:00:00", "10:00:00", "away"),
        ];
        let stats = segment_stats(&segments, "2026-01-05");
        assert_eq!(stats.active_minutes, 0);
        assert_eq!(stats.idle_minutes, 0);
        assert_eq!(stats.last_activity_at.as_deref(), Some("09:00:00"));
    }

    #[test]
    fn parse_time_seconds_handles_clock_and_garbage() {
        assert_eq!(parse_time_seconds("23:59:59"), 86_399);
        assert_eq!(parse_time_seconds("bad"), 0);
    }

    #[test]
    fn wal_sidecar_keeps_non_ascii_path_intact() {
        let path = PathBuf::from("数据目录").join("worklog.sqlite3");
        assert_eq!(
            wal_sidecar(&path, "-wal"),
            PathBuf::from("数据目录").join("worklog.sqlite3-wal")
        );
    }
}
