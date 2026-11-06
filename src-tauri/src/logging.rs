use std::fs;
use std::io::Write;

/// 日志文件大小上限（2 MB），超过就滚动一份 .1，避免无限增长。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 把一条运行日志追加到数据目录的 logs/app.log。
/// 任何失败都静默处理：日志本身不能反过来影响主流程。
pub fn write(level: &str, message: &str) {
    let Some(base) = dirs::data_local_dir() else {
        return;
    };
    let dir = base.join("activity-worklog").join("logs");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("app.log");
    let rotate = fs::metadata(&path)
        .map(|meta| meta.len() >= MAX_LOG_BYTES)
        .unwrap_or(false);
    if rotate {
        let _ = fs::rename(&path, dir.join("app.log.1"));
    }

    let line = format!(
        "{} [{level}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        message.replace('\n', " ")
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}
