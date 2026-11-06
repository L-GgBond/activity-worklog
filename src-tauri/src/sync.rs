use crate::db::{ActivitySegment, DailyRecord};
use crate::settings::AppSettings;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncPayload {
    pub device_id: String,
    pub records: Vec<DailyRecord>,
}

/// 服务端返回的日汇总（不含 sync_status，落库时统一标记为 synced）。
#[derive(Debug, Clone, serde::Deserialize)]
struct PulledRecord {
    date: String,
    #[serde(default)]
    first_activity_at: Option<String>,
    #[serde(default)]
    last_activity_at: Option<String>,
    #[serde(default)]
    active_minutes: i64,
    #[serde(default)]
    idle_minutes: i64,
}

#[derive(Debug, serde::Deserialize)]
struct PullResponse {
    #[serde(default)]
    records: Vec<PulledRecord>,
}

#[derive(Debug, serde::Deserialize)]
struct RemoteSegment {
    #[serde(default)]
    device_id: String,
    date: String,
    started_at: String,
    ended_at: String,
    segment_type: String,
}

#[derive(Debug, serde::Deserialize)]
struct SegmentPullResponse {
    #[serde(default)]
    segments: Vec<RemoteSegment>,
}

/// 基于 reqwest 的同步客户端，负责把待同步记录推送到 Web 后台。
pub struct HttpSyncClient {
    client: reqwest::Client,
}

impl HttpSyncClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    /// 上传待同步记录；返回服务端确认接受的日期列表。
    pub async fn push_records(
        &self,
        settings: &AppSettings,
        records: Vec<DailyRecord>,
    ) -> anyhow::Result<Vec<String>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let sent: Vec<String> = records.iter().map(|record| record.date.clone()).collect();
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let endpoint = format!("{url}/api/sync/records");

        let payload = SyncPayload {
            device_id: device_id(),
            records,
        };

        let mut request = self.client.post(&endpoint).json(&payload);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }

        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("同步服务器返回 {status}: {body}");
        }

        // 服务端应返回 {"accepted": ["2026-11-01", ...]} 逐条确认；
        // 显式给了该字段就按确认结果标记（哪怕为空，表示全部被拒、保留待同步），
        // 只有完全没给字段时才按“全部接受”兼容旧服务端。
        let confirmed = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("accepted").and_then(|item| item.as_array()))
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|value| value.to_string()))
                    .collect::<Vec<String>>()
            });

        match confirmed {
            Some(accepted) => Ok(accepted),
            None => Ok(sent),
        }
    }

    /// 从服务器拉取近 days 天的日汇总：换机、重装、误删本地数据后用来补回历史。
    pub async fn pull_records(
        &self,
        settings: &AppSettings,
        days: u32,
    ) -> anyhow::Result<Vec<DailyRecord>> {
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let days = days.clamp(1, 3660);
        let endpoint = format!("{url}/api/sync/records?days={days}");

        let mut request = self.client.get(&endpoint);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("拉取失败，服务器返回 {status}: {body}");
        }

        let body: PullResponse = response.json().await?;
        Ok(body
            .records
            .into_iter()
            .map(|record| DailyRecord {
                date: record.date,
                first_activity_at: record.first_activity_at,
                last_activity_at: record.last_activity_at,
                active_minutes: record.active_minutes,
                idle_minutes: record.idle_minutes,
                sync_status: "synced".to_string(),
            })
            .collect())
    }

    /// 把本地删除的日期同步到服务器，避免换机恢复时把已删掉的记录又拉回来。
    pub async fn delete_records(
        &self,
        settings: &AppSettings,
        dates: Vec<String>,
    ) -> anyhow::Result<Vec<String>> {
        if dates.is_empty() {
            return Ok(Vec::new());
        }
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let endpoint = format!("{url}/api/sync/delete");
        let payload = serde_json::json!({
            "device_id": device_id(),
            "dates": dates,
        });
        let mut request = self.client.post(&endpoint).json(&payload);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("删除同步失败，服务器返回 {status}: {body}");
        }
        let deleted = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("deleted").and_then(|item| item.as_array()))
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|value| value.to_string()))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        Ok(deleted)
    }

    /// 上传时段明细：服务端按 (device_id, date, started_at) 幂等覆盖。
    pub async fn push_segments(
        &self,
        settings: &AppSettings,
        segments: Vec<ActivitySegment>,
    ) -> anyhow::Result<usize> {
        if segments.is_empty() {
            return Ok(0);
        }
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let endpoint = format!("{url}/api/sync/segments");
        let payload = serde_json::json!({
            "device_id": device_id(),
            "segments": segments.iter().map(|segment| serde_json::json!({
                "date": segment.date,
                "started_at": segment.started_at,
                "ended_at": segment.ended_at,
                "segment_type": segment.segment_type,
            })).collect::<Vec<_>>(),
        });
        let mut request = self.client.post(&endpoint).json(&payload);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("时段同步失败，服务器返回 {status}: {body}");
        }
        let accepted = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("accepted").and_then(|item| item.as_u64()))
            .unwrap_or(0);
        Ok(accepted as usize)
    }

    /// 拉取时段明细并按日期挑选“当天工作时长最多”的设备，
    /// 避免把两台机器同一天的时段拼到一起造成重复计算。
    pub async fn pull_segments(
        &self,
        settings: &AppSettings,
        days: u32,
    ) -> anyhow::Result<Vec<ActivitySegment>> {
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let days = days.clamp(1, 3660);
        let endpoint = format!("{url}/api/sync/segments?days={days}");
        let mut request = self.client.get(&endpoint);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("时段拉取失败，服务器返回 {status}: {body}");
        }
        let body: SegmentPullResponse = response.json().await?;

        // 按 (日期, 设备) 分组，先算每组的工作秒数，再选出每天的胜出设备
        let mut groups: std::collections::BTreeMap<(String, String), Vec<RemoteSegment>> =
            std::collections::BTreeMap::new();
        for segment in body.segments {
            groups
                .entry((segment.date.clone(), segment.device_id.clone()))
                .or_default()
                .push(segment);
        }
        let mut by_date: std::collections::BTreeMap<String, (i64, Vec<RemoteSegment>)> =
            std::collections::BTreeMap::new();
        for ((date, _device), list) in groups {
            let work_seconds = list
                .iter()
                .filter(|segment| segment.segment_type == "work")
                .map(|segment| span_seconds(&segment.started_at, &segment.ended_at))
                .sum::<i64>();
            let entry = by_date.entry(date).or_insert((-1, Vec::new()));
            if work_seconds > entry.0 {
                *entry = (work_seconds, list);
            }
        }
        Ok(by_date
            .into_values()
            .flat_map(|(_, list)| list)
            .map(|segment| ActivitySegment {
                date: segment.date,
                started_at: segment.started_at,
                ended_at: segment.ended_at,
                segment_type: segment.segment_type,
            })
            .collect())
    }

    /// 测试连接：GET /api/health，校验 Token 与服务可达性。
    pub async fn test_connection(&self, settings: &AppSettings) -> anyhow::Result<String> {
        let url = normalize_url(&settings.sync_url);
        if url.is_empty() {
            anyhow::bail!("未配置同步服务器地址");
        }
        let endpoint = format!("{url}/api/health");
        let mut request = self.client.get(&endpoint);
        if !settings.sync_token.is_empty() {
            request = request.bearer_auth(&settings.sync_token);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("服务器返回 {status}，请检查地址或 Token");
        }
        let body = response.text().await.unwrap_or_default();
        Ok(format!("连接成功：{body}"))
    }
}

fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn span_seconds(started: &str, ended: &str) -> i64 {
    let to_seconds = |time: &str| -> i64 {
        chrono::NaiveTime::parse_from_str(time, "%H:%M:%S")
            .map(|parsed| parsed.hour() as i64 * 3600 + parsed.minute() as i64 * 60 + parsed.second() as i64)
            .unwrap_or(0)
    };
    (to_seconds(ended) - to_seconds(started)).max(0)
}

fn device_id() -> String {
    // 用本地数据目录路径的哈希作为稳定的设备标识，避免采集主机名等敏感信息
    let base = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("activity-worklog")
        .to_string_lossy()
        .to_string();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in base.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("device-{hash:016x}")
}
