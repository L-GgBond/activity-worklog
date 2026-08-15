import React, { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Activity,
  Clock3,
  Database,
  Download,
  Moon,
  Power,
  RefreshCcw,
  ShieldCheck
} from "lucide-react";
import "./styles.css";

type DailyRecord = {
  date: string;
  first_activity_at: string | null;
  last_activity_at: string | null;
  active_minutes: number;
  idle_minutes: number;
  sync_status: string;
};

type Segment = {
  date: string;
  started_at: string;
  ended_at: string;
  segment_type: "work" | "away";
};

type Settings = {
  idle_threshold_minutes: number;
  autostart_enabled: boolean;
  sync_enabled: boolean;
  sync_url: string;
  sync_token: string;
  sync_interval_minutes: number;
  onboarded: boolean;
};

type Page = "records" | "sync" | "privacy";

const defaultSettings: Settings = {
  idle_threshold_minutes: 5,
  autostart_enabled: false,
  sync_enabled: false,
  sync_url: "",
  sync_token: "",
  sync_interval_minutes: 15,
  onboarded: false
};

const fallbackRecords: DailyRecord[] = [
  {
    date: "2026-08-08",
    first_activity_at: "09:18",
    last_activity_at: "18:42",
    active_minutes: 421,
    idle_minutes: 143,
    sync_status: "pending"
  }
];

const fallbackSegments: Segment[] = [
  { date: "2026-08-08", started_at: "09:18:00", ended_at: "12:05:00", segment_type: "work" },
  { date: "2026-08-08", started_at: "12:05:00", ended_at: "13:20:00", segment_type: "away" },
  { date: "2026-08-08", started_at: "13:20:00", ended_at: "18:42:00", segment_type: "work" }
];

function formatMinutes(minutes: number) {
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return `${h}小时${m.toString().padStart(2, "0")}分`;
}

function timeToSeconds(time: string) {
  const [h, m, s] = time.split(":").map(Number);
  return (h || 0) * 3600 + (m || 0) * 60 + (s || 0);
}

function shortTime(time: string) {
  return time.slice(0, 5);
}

function App() {
  const [records, setRecords] = useState<DailyRecord[]>([]);
  const [segments, setSegments] = useState<Segment[]>([]);
  const [settings, setSettings] = useState<Settings>(defaultSettings);
  const [status, setStatus] = useState("正在读取本地记录");
  const [page, setPage] = useState<Page>("records");
  const [ready, setReady] = useState(false);
  const [syncTestResult, setSyncTestResult] = useState("");

  const today = records[0] ?? fallbackRecords[0];
  const todayStr = today.date;

  const refresh = useCallback(async () => {
    try {
      const [loadedRecords, loadedSettings] = await Promise.all([
        invoke<DailyRecord[]>("list_daily_records"),
        invoke<Settings>("get_settings")
      ]);
      setRecords(loadedRecords);
      setSettings(loadedSettings);
      setStatus("后台监听已开启");
      return loadedSettings;
    } catch {
      setRecords(fallbackRecords);
      setSegments(fallbackSegments);
      setStatus("预览模式：运行 Tauri 后连接本地服务");
      return null;
    }
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    (async () => {
      await refresh();
      setReady(true);
      try {
        unlisten = await listen("records-updated", () => refresh());
      } catch {
        // 预览模式无事件通道
      }
    })();
    return () => unlisten?.();
  }, [refresh]);

  useEffect(() => {
    if (!todayStr) return;
    invoke<Segment[]>("list_segments", { date: todayStr })
      .then(setSegments)
      .catch(() => setSegments(fallbackSegments));
  }, [todayStr, records]);

  async function updateSettings(next: Partial<Settings>, silent = false) {
    const merged = { ...settings, ...next };
    setSettings(merged);
    try {
      await invoke("save_settings", { settings: merged });
      if (!silent) setStatus("设置已保存");
    } catch {
      if (!silent) setStatus("预览模式：设置将在桌面端保存");
    }
    return merged;
  }

  async function finishOnboarding() {
    await updateSettings({ onboarded: true });
    setStatus("已完成首次设置");
  }

  async function exportCsv() {
    try {
      await invoke("export_records_csv");
      setStatus("CSV 已导出到本地数据目录");
    } catch {
      setStatus("预览模式：桌面端可导出 CSV");
    }
  }

  async function testSync() {
    setSyncTestResult("正在测试连接…");
    try {
      const result = await invoke<string>("test_sync_connection", { settings });
      setSyncTestResult(result);
    } catch (err) {
      setSyncTestResult(`连接失败：${err}`);
    }
  }

  if (!ready) {
    return (
      <main className="shell">
        <section className="workspace">加载中…</section>
      </main>
    );
  }

  if (!settings.onboarded) {
    return <Onboarding settings={settings} onChange={updateSettings} onFinish={finishOnboarding} />;
  }

  return (
    <main className="shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brandMark">
            <Activity size={22} />
          </div>
          <div>
            <strong>Activity Worklog</strong>
            <span>本地隐私工作记录</span>
          </div>
        </div>
        <nav>
          <button
            className={page === "records" ? "navActive" : ""}
            onClick={() => setPage("records")}
          >
            <Clock3 size={18} />每日记录
          </button>
          <button
            className={page === "sync" ? "navActive" : ""}
            onClick={() => setPage("sync")}
          >
            <Database size={18} />数据同步
          </button>
          <button
            className={page === "privacy" ? "navActive" : ""}
            onClick={() => setPage("privacy")}
          >
            <ShieldCheck size={18} />隐私设置
          </button>
        </nav>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p>{status}</p>
            <h1>
              {page === "records" && "自动统计上下班与工作时段"}
              {page === "sync" && "数据同步设置"}
              {page === "privacy" && "隐私与记录设置"}
            </h1>
          </div>
          {page === "records" && (
            <div className="actions">
              <button title="刷新" onClick={refresh}><RefreshCcw size={18} /></button>
              <button title="导出 CSV" onClick={exportCsv}><Download size={18} /></button>
            </div>
          )}
        </header>

        {page === "records" && (
          <>
            <section className="metrics">
              <article className="metric">
                <Clock3 size={19} />
                <span>今日开始</span>
                <strong>{today.first_activity_at ?? "--:--"}</strong>
              </article>
              <article className="metric">
                <Moon size={19} />
                <span>今日结束</span>
                <strong>{today.last_activity_at ?? "--:--"}</strong>
              </article>
              <article className="metric">
                <Activity size={19} />
                <span>活跃时长</span>
                <strong>{formatMinutes(today.active_minutes)}</strong>
              </article>
              <article className="metric">
                <ShieldCheck size={19} />
                <span>休眠阈值</span>
                <strong>{settings.idle_threshold_minutes}分钟</strong>
              </article>
            </section>

            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>今日时间线</h2>
                  <p>绿色为工作时段，橙色为离开时段（{today.date}）</p>
                </div>
              </div>
              <div className="timeline">
                {segments.length === 0 && <p className="empty">今日暂无记录</p>}
                {segments.map((segment, index) => (
                  <div className={`timelineBar ${segment.segment_type}`} key={index}>
                    <div
                      className="timelineFill"
                      style={{
                        left: `${(timeToSeconds(segment.started_at) / 86400) * 100}%`,
                        width: `${Math.max(
                          ((timeToSeconds(segment.ended_at) - timeToSeconds(segment.started_at)) /
                            86400) * 100,
                          0.3
                        )}%`
                      }}
                    />
                  </div>
                ))}
                <div className="timelineLabels">
                  <span>00:00</span>
                  <span>06:00</span>
                  <span>12:00</span>
                  <span>18:00</span>
                  <span>24:00</span>
                </div>
                <div className="segmentList">
                  {segments.map((segment, index) => (
                    <span className={`chip ${segment.segment_type}`} key={index}>
                      {segment.segment_type === "work" ? "工作" : "离开"}{" "}
                      {shortTime(segment.started_at)}–{shortTime(segment.ended_at)}
                    </span>
                  ))}
                </div>
              </div>
            </section>

            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>历史记录</h2>
                  <p>共 {records.length || fallbackRecords.length} 天，本地保存，可按需同步数据库</p>
                </div>
              </div>
              <div className="table">
                <div className="row head">
                  <span>日期</span>
                  <span>开始</span>
                  <span>结束</span>
                  <span>活跃</span>
                  <span>空闲</span>
                  <span>同步</span>
                </div>
                {(records.length ? records : fallbackRecords).map((record) => (
                  <div className="row" key={record.date}>
                    <span>{record.date}</span>
                    <span>{record.first_activity_at ?? "--:--"}</span>
                    <span>{record.last_activity_at ?? "--:--"}</span>
                    <span>{formatMinutes(record.active_minutes)}</span>
                    <span>{formatMinutes(record.idle_minutes)}</span>
                    <span className={`sync ${record.sync_status}`}>{record.sync_status}</span>
                  </div>
                ))}
              </div>
            </section>
          </>
        )}

        {page === "sync" && (
          <section className="panel syncPanel">
            <div className="panelTitle">
              <div>
                <h2>数据库同步</h2>
                <p>记录先保存本地，再按设定频率推送到后台服务器</p>
              </div>
            </div>
            <div className="form">
              <label className="switch">
                <span><Database size={17} />启用同步</span>
                <input
                  type="checkbox"
                  checked={settings.sync_enabled}
                  onChange={(event) => updateSettings({ sync_enabled: event.target.checked })}
                />
              </label>
              <label>
                <span>服务器地址</span>
                <input
                  type="text"
                  placeholder="http://127.0.0.1:8787"
                  value={settings.sync_url}
                  onChange={(event) => updateSettings({ sync_url: event.target.value })}
                />
              </label>
              <label>
                <span>访问 Token</span>
                <input
                  type="password"
                  placeholder="与服务器端保持一致"
                  value={settings.sync_token}
                  onChange={(event) => updateSettings({ sync_token: event.target.value })}
                />
              </label>
              <label>
                <span>同步频率（分钟）</span>
                <input
                  type="number"
                  min={1}
                  max={1440}
                  value={settings.sync_interval_minutes}
                  onChange={(event) =>
                    updateSettings({ sync_interval_minutes: Number(event.target.value) || 15 })
                  }
                />
              </label>
              <div className="formActions">
                <button className="primary" onClick={testSync}>测试连接</button>
                {syncTestResult && <span className="syncResult">{syncTestResult}</span>}
              </div>
            </div>
          </section>
        )}

        {page === "privacy" && (
          <>
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>隐私说明</h2>
                  <p>本软件只判断“有没有操作”，不采集任何内容</p>
                </div>
              </div>
              <ul className="privacyList">
                <li>不读取具体按键，不保存键盘输入内容</li>
                <li>不保存鼠标坐标，不截图，不读取用户文件</li>
                <li>默认只保存活动时间、统计分钟数与同步状态</li>
                <li>数据保存在本机，只有开启同步后才上传服务器</li>
              </ul>
            </section>
            <section className="settings">
              <label>
                <span>离开判定</span>
                <input
                  type="range"
                  min="1"
                  max="60"
                  value={settings.idle_threshold_minutes}
                  onChange={(event) =>
                    updateSettings({ idle_threshold_minutes: Number(event.target.value) })
                  }
                />
                <strong>{settings.idle_threshold_minutes} 分钟</strong>
              </label>
              <label className="switch">
                <span><Power size={17} />开机自启</span>
                <input
                  type="checkbox"
                  checked={settings.autostart_enabled}
                  onChange={(event) => updateSettings({ autostart_enabled: event.target.checked })}
                />
              </label>
            </section>
          </>
        )}
      </section>
    </main>
  );
}

function Onboarding({
  settings,
  onChange,
  onFinish
}: {
  settings: Settings;
  onChange: (next: Partial<Settings>, silent?: boolean) => Promise<Settings>;
  onFinish: () => void;
}) {
  const [step, setStep] = useState(0);
  const steps = ["隐私说明", "数据同步", "开机自启"];

  return (
    <main className="wizard">
      <section className="wizardCard">
        <div className="brand">
          <div className="brandMark">
            <Activity size={22} />
          </div>
          <div>
            <strong>Activity Worklog</strong>
            <span>首次设置向导</span>
          </div>
        </div>

        <ol className="wizardSteps">
          {steps.map((label, index) => (
            <li key={label} className={index === step ? "active" : index < step ? "done" : ""}>
              {label}
            </li>
          ))}
        </ol>

        {step === 0 && (
          <div className="wizardBody">
            <h2>我们如何保护你的隐私</h2>
            <ul className="privacyList">
              <li>只监听系统空闲时长，判断“有没有操作”，不读取按键内容</li>
              <li>不保存键盘输入、鼠标坐标，不截图，不读取用户文件</li>
              <li>所有记录默认只保存在本机数据库</li>
              <li>只有你主动开启同步，数据才会上传服务器</li>
            </ul>
          </div>
        )}

        {step === 1 && (
          <div className="wizardBody">
            <h2>配置数据同步（可选）</h2>
            <p className="hint">可先跳过，稍后在“数据同步”页面再配置。</p>
            <div className="form">
              <label className="switch">
                <span><Database size={17} />启用同步</span>
                <input
                  type="checkbox"
                  checked={settings.sync_enabled}
                  onChange={(event) => onChange({ sync_enabled: event.target.checked }, true)}
                />
              </label>
              <label>
                <span>服务器地址</span>
                <input
                  type="text"
                  placeholder="http://127.0.0.1:8787"
                  value={settings.sync_url}
                  onChange={(event) => onChange({ sync_url: event.target.value }, true)}
                />
              </label>
              <label>
                <span>访问 Token</span>
                <input
                  type="password"
                  placeholder="与服务器端保持一致"
                  value={settings.sync_token}
                  onChange={(event) => onChange({ sync_token: event.target.value }, true)}
                />
              </label>
            </div>
          </div>
        )}

        {step === 2 && (
          <div className="wizardBody">
            <h2>开机自动启动</h2>
            <p className="hint">开启后每次开机自动在后台运行，可随时在设置中关闭。</p>
            <div className="form">
              <label className="switch">
                <span><Power size={17} />开机自启</span>
                <input
                  type="checkbox"
                  checked={settings.autostart_enabled}
                  onChange={(event) => onChange({ autostart_enabled: event.target.checked }, true)}
                />
              </label>
            </div>
          </div>
        )}

        <div className="wizardActions">
          {step > 0 && (
            <button onClick={() => setStep(step - 1)}>上一步</button>
          )}
          {step < steps.length - 1 && (
            <button className="primary" onClick={() => setStep(step + 1)}>下一步</button>
          )}
          {step === steps.length - 1 && (
            <button className="primary" onClick={onFinish}>完成设置</button>
          )}
        </div>
      </section>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
