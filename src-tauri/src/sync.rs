use crate::db::DailyRecord;

pub trait WorklogSyncClient {
    fn push_daily_records(&self, records: &[DailyRecord]) -> anyhow::Result<()>;
}

pub struct DisabledSyncClient;

impl WorklogSyncClient for DisabledSyncClient {
    fn push_daily_records(&self, _records: &[DailyRecord]) -> anyhow::Result<()> {
        Ok(())
    }
}
