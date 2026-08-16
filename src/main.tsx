import React, { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Activity,
  BarChart3,
  Clock3,
  Database,
  Download,
  Moon,
  Power,
  RefreshCcw,
  ShieldCheck,
  TrendingUp
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
  work_start: string;
  work_end: string;
  lunch_start: string;
  lunch_end: string;
};

type Page = "records" | "analytics" | "sync" | "privacy";

const defaultSettings: Settings = {
  idle_threshold_minutes: 5,
  autostart_enabled: false,
  sync_enabled: false,
  sync_url: "",
  sync_token: "",
  sync_interval_minutes: 15,
  onboarded: false,
  work_start: "09:30",
  work_end: "18:30",
  lunch_start: "12:00",
  lunch_end: "13:30"
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
            className={page === "analytics" ? "navActive" : ""}
            onClick={() => setPage("analytics")}
          >
            <BarChart3 size={18} />数据分析
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
        <div className="sidebarFoot">
          <span className="liveDot" />
          后台监听已开启
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p>{status}</p>
            <h1>
              {page === "records" && "自动统计上下班与工作时段"}
              {page === "analytics" && "数据统计与分析"}
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
            <section className="hero">
              <div className="heroRing">
                <TodayRing active={today.active_minutes} idle={today.idle_minutes} />
              </div>
              <div className="heroStats">
                <div className="heroStat">
                  <span><Clock3 size={16} />今日开始</span>
                  <strong>{today.first_activity_at ?? "--:--"}</strong>
                </div>
                <div className="heroStat">
                  <span><Moon size={16} />今日结束</span>
                  <strong>{today.last_activity_at ?? "--:--"}</strong>
                </div>
                <div className="heroStat">
                  <span><Activity size={16} />活跃时长</span>
                  <strong>{formatMinutes(today.active_minutes)}</strong>
                </div>
                <div className="heroStat">
                  <span><TrendingUp size={16} />活跃占比</span>
                  <strong>
                    {today.active_minutes + today.idle_minutes > 0
                      ? Math.round(
                          (today.active_minutes /
                            (today.active_minutes + today.idle_minutes)) *
                            100
                        )
                      : 0}
                    %
                  </strong>
                </div>
              </div>
            </section>

            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>今日时间线</h2>
                  <p>绿色为工作时段，橙色为离开时段（{today.date}）</p>
                </div>
              </div>
              <div className="timeline">
                <div className="legend">
                  <span><i className="work" />工作时段</span>
                  <span><i className="away" />离开时段</span>
                </div>
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
                  {segments.map((segment, index) => {
                    const dur = Math.max(
                      Math.round(
                        (timeToSeconds(segment.ended_at) -
                          timeToSeconds(segment.started_at)) /
                          60
                      ),
                      0
                    );
                    return (
                      <span className={`chip ${segment.segment_type}`} key={index}>
                        {segment.segment_type === "work" ? "工作" : "离开"}{" "}
                        {shortTime(segment.started_at)}–{shortTime(segment.ended_at)} ·{" "}
                        {dur}分钟
                      </span>
                    );
                  })}
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

        {page === "analytics" && (
          <AnalyticsPage
            records={records.length ? records : fallbackRecords}
            settings={settings}
          />
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
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>作息时间</h2>
                  <p>按你的实际上下班与休息时间设置，报表会据此对比</p>
                </div>
              </div>
              <div className="scheduleGrid">
                <label>
                  <span>上班时间</span>
                  <input
                    type="time"
                    value={settings.work_start}
                    onChange={(event) => updateSettings({ work_start: event.target.value })}
                  />
                </label>
                <label>
                  <span>下班时间</span>
                  <input
                    type="time"
                    value={settings.work_end}
                    onChange={(event) => updateSettings({ work_end: event.target.value })}
                  />
                </label>
                <label>
                  <span>午餐开始（吃饭）</span>
                  <input
                    type="time"
                    value={settings.lunch_start}
                    onChange={(event) => updateSettings({ lunch_start: event.target.value })}
                  />
                </label>
                <label>
                  <span>下午上班时间</span>
                  <input
                    type="time"
                    value={settings.lunch_end}
                    onChange={(event) => updateSettings({ lunch_end: event.target.value })}
                  />
                </label>
              </div>
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
              <span className="dot" />
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

/* ---------------- 图表组件（无第三方依赖，手写 SVG） ---------------- */

function parseHM(time: string) {
  const [h, m] = (time || "00:00").split(":").map(Number);
  return (h || 0) * 3600 + (m || 0) * 60;
}

/** 近 7 天活跃时长柱状图 */
function MiniBarChart({ records }: { records: DailyRecord[] }) {
  const days = records.slice(0, 7).reverse();
  const max = Math.max(...days.map((d) => d.active_minutes), 60);
  const W = 340;
  const H = 168;
  const padB = 26;
  const padT = 20;
  const chartH = H - padB - padT;
  const bw = W / days.length;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="近7天活跃时长柱状图">
      {[0.25, 0.5, 0.75, 1].map((r) => (
        <line
          key={r}
          x1={0}
          x2={W}
          y1={padT + chartH * (1 - r)}
          y2={padT + chartH * (1 - r)}
          className="gridLine"
        />
      ))}
      {days.map((d, i) => {
        const h = Math.max((d.active_minutes / max) * chartH, 4);
        const x = i * bw + bw * 0.18;
        const w = bw * 0.64;
        const y = padT + chartH - h;
        return (
          <g key={d.date}>
            <rect x={x} y={y} width={w} height={h} rx={5} className="bar" />
            <text x={x + w / 2} y={y - 6} textAnchor="middle" className="barLabel">
              {(d.active_minutes / 60).toFixed(1)}h
            </text>
            <text x={x + w / 2} y={H - 8} textAnchor="middle" className="barDate">
              {d.date.slice(5)}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

/** 作息对比：每天的实际活跃区间 vs 计划工作时段 */
function ScheduleStrip({ days, plan }: { days: DailyRecord[]; plan: Settings }) {
  const rows = days.slice(0, 5);
  const W = 340;
  const rowH = 44;
  const H = rows.length * rowH + 16;
  const padL = 56;
  const padR = 14;
  const x = (sec: number) => padL + (sec / 86400) * (W - padL - padR);

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="作息对比图">
      {rows.map((d, i) => {
        const y = i * rowH + 6;
        const rh = 30;
        const planA = Math.max(x(parseHM(plan.lunch_start)) - x(parseHM(plan.work_start)), 0);
        const planB = Math.max(x(parseHM(plan.work_end)) - x(parseHM(plan.lunch_end)), 0);
        const start = d.first_activity_at ? parseHM(d.first_activity_at) : 0;
        const end = d.last_activity_at ? parseHM(d.last_activity_at) : 0;
        return (
          <g key={d.date}>
            <rect x={x(parseHM(plan.work_start))} y={y} width={planA} height={rh} rx={6} className="planA" />
            <rect x={x(parseHM(plan.lunch_end))} y={y} width={planB} height={rh} rx={6} className="planB" />
            {end > start && (
              <rect x={x(start)} y={y + 7} width={Math.max(x(end) - x(start), 3)} height={rh - 14} rx={6} className="actualBand" />
            )}
            <text x={padL - 8} y={y + rh / 2 + 4} textAnchor="end" className="rowDate">
              {d.date.slice(5)}
            </text>
          </g>
        );
      })}
      {[0, 6, 12, 18, 24].map((h) => (
        <text key={h} x={x(h * 3600)} y={H - 3} textAnchor="middle" className="tickLabel">
          {String(h).padStart(2, "0")}:00
        </text>
      ))}
    </svg>
  );
}

/** 活跃 / 空闲占比环形图 */
function DonutChart({ records }: { records: DailyRecord[] }) {
  const days = records.slice(0, 7);
  const active = days.reduce((s, d) => s + d.active_minutes, 0);
  const idle = days.reduce((s, d) => s + d.idle_minutes, 0);
  const total = active + idle || 1;
  const pct = Math.round((active / total) * 100);
  const R = 52;
  const C = 2 * Math.PI * R;
  const dash = (pct / 100) * C;

  return (
    <div className="donutWrap">
      <svg viewBox="0 0 140 140" className="donut" role="img" aria-label="活跃空闲占比环形图">
        <circle cx={70} cy={70} r={R} className="donutTrack" />
        <circle
          cx={70}
          cy={70}
          r={R}
          className="donutActive"
          strokeDasharray={`${dash} ${C - dash}`}
          transform="rotate(-90 70 70)"
        />
        <circle
          cx={70}
          cy={70}
          r={R}
          className="donutIdle"
          strokeDasharray={`${C - dash} ${dash}`}
          transform={`rotate(${-90 + (pct / 100) * 360} 70 70)`}
        />
        <text x={70} y={63} textAnchor="middle" className="donutPct">{pct}%</text>
        <text x={70} y={82} textAnchor="middle" className="donutLabel">活跃占比</text>
      </svg>
      <div className="donutLegend">
        <span><i className="work" />活跃 {formatMinutes(active)}</span>
        <span><i className="away" />空闲 {formatMinutes(idle)}</span>
      </div>
    </div>
  );
}

/* ---------------- 今日概览环形 ---------------- */
function TodayRing({ active, idle }: { active: number; idle: number }) {
  const total = active + idle || 1;
  const pct = Math.round((active / total) * 100);
  const R = 58;
  const C = 2 * Math.PI * R;
  const dash = (pct / 100) * C;
  return (
    <svg viewBox="0 0 160 160" className="ring" role="img" aria-label="今日活跃占比">
      <circle cx={80} cy={80} r={R} className="ringTrack" />
      <circle
        cx={80}
        cy={80}
        r={R}
        className="ringActive"
        strokeDasharray={`${dash} ${C - dash}`}
        transform="rotate(-90 80 80)"
      />
      <text x={80} y={74} textAnchor="middle" className="ringPct">{pct}%</text>
      <text x={80} y={98} textAnchor="middle" className="ringLabel">今日活跃</text>
    </svg>
  );
}

function fmtClock(sec: number) {
  const h = Math.floor(sec / 3600) % 24;
  const m = Math.floor((sec % 3600) / 60);
  return `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}`;
}

/* ---------------- 独立数据分析页 ---------------- */
function AnalyticsPage({ records, settings }: { records: DailyRecord[]; settings: Settings }) {
  const [range, setRange] = useState(7);
  const data = records.length ? records : fallbackRecords;
  const slice = data.slice(0, range);
  const days = slice.length;

  const totalActive = slice.reduce((s, d) => s + d.active_minutes, 0);
  const totalIdle = slice.reduce((s, d) => s + d.idle_minutes, 0);
  const avgActive = days ? Math.round(totalActive / days) : 0;
  const avgIdle = days ? Math.round(totalIdle / days) : 0;
  const eff = avgActive + avgIdle > 0 ? Math.round((avgActive / (avgActive + avgIdle)) * 100) : 0;

  const starts = slice
    .filter((d) => d.first_activity_at)
    .map((d) => parseHM(d.first_activity_at as string));
  const ends = slice
    .filter((d) => d.last_activity_at)
    .map((d) => parseHM(d.last_activity_at as string));
  const avgStart = starts.length ? Math.round(starts.reduce((a, b) => a + b, 0) / starts.length) : 0;
  const avgEnd = ends.length ? Math.round(ends.reduce((a, b) => a + b, 0) / ends.length) : 0;

  const ws = parseHM(settings.work_start);
  const we = parseHM(settings.work_end);
  const achieved = slice.filter((d) => {
    if (!d.first_activity_at || !d.last_activity_at) return false;
    return (
      parseHM(d.first_activity_at) <= ws + 1800 &&
      parseHM(d.last_activity_at) >= we - 1800
    );
  }).length;
  const achieveRate = days ? Math.round((achieved / days) * 100) : 0;

  const ranges = [
    { label: "近 7 天", value: 7 },
    { label: "近 30 天", value: 30 },
    { label: "全部", value: 90 },
  ];

  return (
    <div className="analytics">
      <div className="tabs">
        {ranges.map((r) => (
          <button
            key={r.value}
            className={range === r.value ? "tab active" : "tab"}
            onClick={() => setRange(r.value)}
          >
            {r.label}
          </button>
        ))}
      </div>

      <section className="kpiGrid">
        <div className="kpi">
          <span>总活跃时长</span>
          <strong>{formatMinutes(totalActive)}</strong>
        </div>
        <div className="kpi">
          <span>日均活跃</span>
          <strong>{formatMinutes(avgActive)}</strong>
        </div>
        <div className="kpi">
          <span>平均效率</span>
          <strong>{eff}%</strong>
        </div>
        <div className="kpi">
          <span>平均上班</span>
          <strong>{fmtClock(avgStart)}</strong>
        </div>
        <div className="kpi">
          <span>平均下班</span>
          <strong>{fmtClock(avgEnd)}</strong>
        </div>
        <div className="kpi">
          <span>计划达成率</span>
          <strong>{achieveRate}%</strong>
        </div>
      </section>

      {days < 3 && (
        <p className="hint">累计记录天数较少，多用几天后统计会更准确。</p>
      )}

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>活跃时长趋势</h2>
            <p>最近 {days} 天每日活跃时长</p>
          </div>
        </div>
        <div className="charts">
          <div className="chartCard wide">
            <MiniBarChart records={slice} />
          </div>
        </div>
      </section>

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>作息与占比</h2>
            <p>
              实际活跃区间 vs 计划（{settings.work_start}–{settings.lunch_start} /{" "}
              {settings.lunch_end}–{settings.work_end}）
            </p>
          </div>
        </div>
        <div className="charts">
          <div className="chartCard">
            <h3>作息对比 · 实际 vs 计划</h3>
            <ScheduleStrip days={slice} plan={settings} />
          </div>
          <div className="chartCard">
            <h3>活跃 / 空闲占比</h3>
            <DonutChart records={slice} />
          </div>
        </div>
      </section>

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>星期分布</h2>
            <p>一周各天平均活跃时长</p>
          </div>
        </div>
        <div className="chartCard">
          <WeekdayChart records={slice} />
        </div>
      </section>
    </div>
  );
}

/** 一周各天平均活跃柱状图（周末用暖色区分） */
function WeekdayChart({ records }: { records: DailyRecord[] }) {
  const names = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
  const buckets: number[][] = Array.from({ length: 7 }, () => []);
  for (const d of records) {
    const dt = new Date(`${d.date}T00:00:00`);
    buckets[dt.getDay()].push(d.active_minutes);
  }
  const avg = buckets.map((b) => (b.length ? b.reduce((a, c) => a + c, 0) / b.length : 0));
  const max = Math.max(...avg, 60);
  const W = 340;
  const H = 172;
  const padB = 26;
  const padT = 20;
  const chartH = H - padB - padT;
  const bw = W / 7;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="星期分布柱状图">
      {[0.25, 0.5, 0.75, 1].map((r) => (
        <line
          key={r}
          x1={0}
          x2={W}
          y1={padT + chartH * (1 - r)}
          y2={padT + chartH * (1 - r)}
          className="gridLine"
        />
      ))}
      {avg.map((v, i) => {
        const h = Math.max((v / max) * chartH, 2);
        const x = i * bw + bw * 0.2;
        const w = bw * 0.6;
        const y = padT + chartH - h;
        const isWeekend = i === 0 || i === 6;
        return (
          <g key={i}>
            <rect x={x} y={y} width={w} height={h} rx={5} className={isWeekend ? "barWeekend" : "bar"} />
            <text x={x + w / 2} y={y - 6} textAnchor="middle" className="barLabel">
              {(v / 60).toFixed(1)}
            </text>
            <text x={x + w / 2} y={H - 8} textAnchor="middle" className="barDate">
              {names[i]}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
