use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Local, Timelike};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::settings::AppSettings;

#[derive(Debug, Clone, Serialize)]
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

pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
}

impl Store {
    pub fn open() -> anyhow::Result<Self> {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("activity-worklog");
        fs::create_dir_all(&data_dir)?;
        let conn = Connection::open(data_dir.join("worklog.sqlite3"))?;
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
    ) -> anyhow::Result<()> {
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

        match current_idx {
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

        // 全量持久化受影响日期的段落，并基于段落重算统计
        for affected in &affected_dates {
            self.persist_day_segments(affected, segments)?;
            let stats = segment_stats(segments, affected);
            let updated_at = timestamp.to_rfc3339();
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
                    affected,
                    stats.first_activity_at,
                    stats.last_activity_at,
                    &updated_at,
                    stats.active_minutes,
                    stats.idle_minutes
                ],
            )?;
        }
        Ok(())
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
                &date,
                stats.first_activity_at,
                stats.last_activity_at,
                &updated_at,
                stats.active_minutes,
                stats.idle_minutes
            ],
        )?;
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

    pub fn list_daily_records(&self) -> anyhow::Result<Vec<DailyRecord>> {
        let mut stmt = self.conn.prepare(
            "
            select date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status
            from daily_work_records
            order by date desc
            limit 90
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
        let path = self.data_dir.join("daily-work-records.csv");
        let mut writer = csv::Writer::from_path(&path)?;
        for record in self.list_daily_records()? {
            writer.serialize(record)?;
        }
        writer.flush()?;
        Ok(path)
    }
}

struct DayStats {
    first_activity_at: Option<String>,
    last_activity_at: Option<String>,
    active_minutes: i64,
    idle_minutes: i64,
}

/// 基于段落数据重算某天的统计数据（按秒累计，彻底修复分钟数恒为 0 的问题）。
fn segment_stats(segments: &[ActivitySegment], date: &str) -> DayStats {
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
