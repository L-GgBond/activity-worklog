use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Local};
use rusqlite::{params, Connection};
use serde::Serialize;

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
            ",
        )?;
        Ok(())
    }

    pub fn record_activity(
        &self,
        timestamp: DateTime<Local>,
        idle_threshold_minutes: u32,
    ) -> anyhow::Result<()> {
        let date = timestamp.format("%Y-%m-%d").to_string();
        let time = timestamp.format("%H:%M:%S").to_string();
        let updated_at = timestamp.to_rfc3339();

        let existing_last: Option<String> = self
            .conn
            .query_row(
                "select last_activity_at from daily_work_records where date = ?1",
                params![&date],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let added_active_minutes = existing_last
            .and_then(|last| {
                chrono::NaiveTime::parse_from_str(&last, "%H:%M:%S")
                    .ok()
                    .map(|last_time| {
                        let current_time = timestamp.time();
                        current_time.signed_duration_since(last_time).num_minutes()
                    })
            })
            .filter(|minutes| *minutes > 0 && *minutes <= i64::from(idle_threshold_minutes))
            .unwrap_or(0);

        self.conn.execute(
            "
            insert into daily_work_records
                (date, first_activity_at, last_activity_at, active_minutes, idle_minutes, sync_status, updated_at)
            values
                (?1, ?2, ?2, 0, 0, 'pending', ?3)
            on conflict(date) do update set
                last_activity_at = excluded.last_activity_at,
                active_minutes = daily_work_records.active_minutes + ?4,
                sync_status = 'pending',
                updated_at = excluded.updated_at
            ",
            params![&date, &time, &updated_at, added_active_minutes],
        )?;
        Ok(())
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
