import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
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
import { toBlob } from "html-to-image";
import { jsPDF } from "jspdf";
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

type HourBucket = {
  hour: number;
  minutes: number;
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
  window_width: number;
  window_height: number;
  theme: "system" | "light" | "dark";
  daily_target_minutes: number;
  menubar_only: boolean;
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
  lunch_end: "13:30",
  window_width: 0,
  window_height: 0,
  theme: "system",
  daily_target_minutes: 480,
  menubar_only: false
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
  const [appVersion, setAppVersion] = useState("");
  const [updateInfo, setUpdateInfo] = useState<{
    status: string;
    current: string;
    latest: string | null;
    notes: string | null;
    url: string | null;
  } | null>(null);
  const [checking, setChecking] = useState(false);

  const settingsRef = useRef(settings);
  settingsRef.current = settings;

  const recordsRef = useRef(records);
  recordsRef.current = records;

  const [exportState, setExportState] = useState<"idle" | "exporting" | "done" | "error">("idle");
  const [exportPath, setExportPath] = useState("");
  const reportRef = useRef<HTMLDivElement>(null);

  const [logLines, setLogLines] = useState<string[]>([]);
  const pushLog = useCallback((msg: string) => {
    const ts = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    setLogLines((prev) => [`${ts}  ${msg}`, ...prev].slice(0, 60));
  }, []);


  const today = records[0] ?? fallbackRecords[0];
  const todayStr = today.date;

  const syncState = syncTestResult
    ? syncTestResult.startsWith("连接成功")
      ? "ok"
      : "err"
    : "";

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

  // 窗口尺寸记忆：启动恢复上次尺寸，拖动停止后保存
  useEffect(() => {
    if (!ready || !settings.onboarded) return;
    const win = getCurrentWindow();
    const w = settingsRef.current.window_width;
    const h = settingsRef.current.window_height;
    if (w >= 960 && h >= 600) {
      win.setSize(new LogicalSize(w, h)).catch(() => {});
    }
    let unlisten: (() => void) | null = null;
    let timer: number | null = null;
    const onResized = async () => {
      try {
        const size = await win.innerSize();
        const sf = await win.scaleFactor();
        const lw = Math.round(size.width / sf);
        const lh = Math.round(size.height / sf);
        if (timer) window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          updateSettings({ window_width: lw, window_height: lh }, true);
        }, 500);
      } catch {
        // 预览模式无窗口对象
      }
    };
    win.onResized(onResized)
      .then((u) => {
        unlisten = u;
      })
      .catch(() => {});
    return () => {
      unlisten?.();
      if (timer) window.clearTimeout(timer);
    };
  }, [ready, settings.onboarded]);

  // 主题应用：system 跟随系统，否则手动 light/dark
  useEffect(() => {
    const root = document.documentElement;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      const eff =
        settingsRef.current.theme === "system"
          ? mq.matches
            ? "dark"
            : "light"
          : settingsRef.current.theme;
      root.setAttribute("data-theme", eff);
    };
    apply();
    if (settingsRef.current.theme === "system") {
      mq.addEventListener("change", apply);
      return () => mq.removeEventListener("change", apply);
    }
  }, [settings.theme]);

  // 读取应用版本（用于关于页）
  useEffect(() => {
    getVersion()
      .then(setAppVersion)
      .catch(() => setAppVersion("0.2.0"));
  }, []);

  // 托盘「今日小结」：显示并聚焦窗口到记录页，顶部展示今日小结
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    (async () => {
      try {
        unlisten = await listen("tray-summary", () => {
          const win = getCurrentWindow();
          win.show().catch(() => {});
          win.setFocus().catch(() => {});
          const t = recordsRef.current[0] ?? fallbackRecords[0];
          const total = t.active_minutes + t.idle_minutes;
          const pct = total > 0 ? Math.round((t.active_minutes / total) * 100) : 0;
          setPage("records");
          setStatus(`今日小结：活跃 ${formatMinutes(t.active_minutes)} · 占比 ${pct}%`);
        });
      } catch {
        // 预览模式无事件通道
      }
    })();
    return () => unlisten?.();
  }, []);

  async function updateSettings(next: Partial<Settings>, silent = false) {
    const merged = { ...settingsRef.current, ...next };
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

  async function handleExport() {
    const node = reportRef.current;
    if (!node) return;
    setExportState("exporting");
    try {
      const bg = window.getComputedStyle(node).backgroundColor || "#ffffff";
      const blob = await toBlob(node, { pixelRatio: 2, cacheBust: true, backgroundColor: bg });
      if (!blob) throw new Error("生成图片失败");
      const arr = new Uint8Array(await blob.arrayBuffer());
      const name = `Activity Worklog 周报 ${new Date().toISOString().slice(0, 10)}`;
      const path = await invoke<string>("export_report", {
        filename: name,
        data: Array.from(arr),
        ext: "png"
      });
      setExportPath(path);
      setExportState("done");
    } catch (e) {
      console.error(e);
      setExportState("error");
    }
  }

  async function exportCsv() {
    try {
      const path = await invoke<string>("export_records_csv");
      pushLog(`已导出 CSV：${path}`);
      setStatus("CSV 已导出到本地数据目录");
    } catch {
      setStatus("预览模式：桌面端可导出 CSV");
    }
  }

  async function handleExportPdf() {
    const node = reportRef.current;
    if (!node) return;
    setExportState("exporting");
    try {
      const bg = window.getComputedStyle(node).backgroundColor || "#ffffff";
      const blob = await toBlob(node, { pixelRatio: 2, cacheBust: true, backgroundColor: bg });
      if (!blob) throw new Error("生成图片失败");
      const reader = new FileReader();
      const imgData: string = await new Promise((resolve, reject) => {
        reader.onload = () => resolve(reader.result as string);
        reader.onerror = reject;
        reader.readAsDataURL(blob);
      });
      const pdf = new jsPDF({ orientation: "portrait", unit: "pt", format: "a4" });
      const pageW = pdf.internal.pageSize.getWidth();
      const pageH = pdf.internal.pageSize.getHeight();
      const imgW = pageW - 40;
      const imgH = Math.min((node.offsetHeight / node.offsetWidth) * imgW, pageH - 40);
      pdf.addImage(imgData, "PNG", 20, 20, imgW, imgH);
      const arr = new Uint8Array(pdf.output("arraybuffer"));
      const name = `Activity Worklog 周报 ${new Date().toISOString().slice(0, 10)}`;
      const path = await invoke<string>("export_report", {
        filename: name,
        data: Array.from(arr),
        ext: "pdf"
      });
      setExportPath(path);
      setExportState("done");
      pushLog(`已导出 PDF：${path}`);
    } catch (e) {
      console.error(e);
      setExportState("error");
    }
  }

  async function testSync() {
    setSyncTestResult("正在测试连接…");
    try {
      const result = await invoke<string>("test_sync_connection", { settings });
      setSyncTestResult(result);
      pushLog(result.startsWith("连接成功") ? `同步测试成功` : `同步测试失败：${result}`);
    } catch (err) {
      setSyncTestResult(`连接失败：${err}`);
      pushLog(`同步测试异常：${err}`);
    }
  }

  async function backupDb() {
    try {
      const path = await invoke<string>("export_db");
      pushLog(`已备份数据：${path}`);
      setStatus(`已备份：${path}`);
    } catch (err) {
      pushLog(`备份失败：${err}`);
      setStatus("预览模式：桌面端可备份数据");
    }
  }

  async function restoreDb() {
    try {
      const msg = await invoke<string>("restore_db");
      pushLog(msg);
      setStatus(msg);
    } catch (err) {
      pushLog(`恢复失败：${err}`);
      setStatus("预览模式：桌面端可恢复数据");
    }
  }

  async function checkForUpdate() {
    setChecking(true);
    setUpdateInfo(null);
    try {
      const info = await invoke<{
        status: string;
        current: string;
        latest: string | null;
        notes: string | null;
        url: string | null;
      }>("check_update");
      setUpdateInfo(info);
      if (info.status === "available") {
        pushLog(`发现新版本 ${info.latest}`);
      } else if (info.status === "latest") {
        pushLog("已是最新版本");
      } else {
        pushLog(`检查更新失败：${info.notes ?? ""}`);
      }
    } catch (err) {
      setUpdateInfo({
        status: "error",
        current: appVersion,
        latest: null,
        notes: String(err),
        url: null
      });
      pushLog(`检查更新异常：${err}`);
    } finally {
      setChecking(false);
    }
  }

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    (async () => {
      try {
        unlisten = await listen<string>("sync-error", (event) => {
          pushLog(`同步出错：${event.payload}`);
        });
      } catch {
        // 预览模式无事件通道
      }
    })();
    return () => unlisten?.();
  }, []);

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
                {segments.map((segment, index) => {
                  const dur = Math.max(
                    Math.round(
                      (timeToSeconds(segment.ended_at) - timeToSeconds(segment.started_at)) / 60
                    ),
                    0
                  );
                  return (
                    <div className={`timelineBar ${segment.segment_type}`} key={index}>
                      <div
                        className="timelineFill"
                        data-tip={`${segment.segment_type === "work" ? "工作" : "离开"} ${shortTime(
                          segment.started_at
                        )}–${shortTime(segment.ended_at)} · ${dur}分钟`}
                        style={{
                          left: `${(timeToSeconds(segment.started_at) / 86400) * 100}%`,
                          width: `${Math.max(
                            ((timeToSeconds(segment.ended_at) - timeToSeconds(segment.started_at)) /
                              86400) *
                              100,
                            0.3
                          )}%`
                        }}
                      />
                    </div>
                  );
                })}
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
            reportRef={reportRef}
            onExport={handleExport}
            onExportCsv={exportCsv}
            onExportPdf={handleExportPdf}
            exportState={exportState}
            exportPath={exportPath}
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
                {syncTestResult && (
                  <span className={`syncResult ${syncState}`}>{syncTestResult}</span>
                )}
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
                  <h2>外观</h2>
                  <p>跟随系统，或手动选择浅色 / 深色</p>
                </div>
              </div>
              <div className="form">
                <div className="tabs">
                  {(["system", "light", "dark"] as const).map((t) => (
                    <button
                      key={t}
                      className={settings.theme === t ? "tab active" : "tab"}
                      onClick={() => updateSettings({ theme: t })}
                    >
                      {t === "system" ? "跟随系统" : t === "light" ? "浅色" : "深色"}
                    </button>
                  ))}
                </div>
              </div>
            </section>
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>窗口与菜单栏</h2>
                  <p>菜单栏驻留模式会隐藏 Dock 图标，仅菜单栏常驻（重启后生效）</p>
                </div>
              </div>
              <div className="form">
                <label className="switch">
                  <span><Power size={17} />菜单栏驻留模式</span>
                  <input
                    type="checkbox"
                    checked={settings.menubar_only}
                    onChange={(event) => updateSettings({ menubar_only: event.target.checked })}
                  />
                </label>
              </div>
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
                <label>
                  <span>每日目标活跃时长</span>
                  <input
                    type="number"
                    min={0}
                    max={1440}
                    step={15}
                    value={settings.daily_target_minutes}
                    onChange={(event) =>
                      updateSettings({ daily_target_minutes: Number(event.target.value) || 0 })
                    }
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
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>关于</h2>
                  <p>本地隐私工作记录 · 数据始终保存在本机</p>
                </div>
              </div>
              <div className="aboutBox">
                <div className="aboutRow">
                  <span>应用名称</span>
                  <strong>Activity Worklog</strong>
                </div>
                <div className="aboutRow">
                  <span>版本</span>
                  <strong>{appVersion || "—"}</strong>
                </div>
                <div className="aboutRow">
                  <span>技术栈</span>
                  <strong>Tauri 2 · React · TypeScript</strong>
                </div>
                <div className="aboutRow">
                  <span>数据存储</span>
                  <strong>本机 SQLite，默认不上传</strong>
                </div>
                <div className="aboutRow">
                  <span>快捷键</span>
                  <strong>⌘R 刷新 · ⌘Q 退出 · ⌘W 关闭窗口</strong>
                </div>
              </div>
            </section>
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>检查更新</h2>
                  <p>当前版本 {appVersion || "—"} · 有新版时提供下载链接</p>
                </div>
              </div>
              <div className="form">
                <div className="formActions">
                  <button onClick={checkForUpdate} disabled={checking}>
                    {checking ? "检查中…" : "检查更新"}
                  </button>
                </div>
                {updateInfo && updateInfo.status === "available" && (
                  <div className="updateBox">
                    <p className="updateNew">
                      发现新版本 {updateInfo.latest}（当前 {updateInfo.current}）
                    </p>
                    {updateInfo.notes && <p className="updateNotes">{updateInfo.notes}</p>}
                    {updateInfo.url && (() => {
                      const releaseUrl = updateInfo.url.replace(
                        /releases\/download\/(v[^\/]+)\/.*/,
                        "releases/tag/$1"
                      );
                      return (
                        <a
                          className="updateLink"
                          href={releaseUrl}
                          target="_blank"
                          rel="noreferrer"
                        >
                          前往下载 / 查看发布
                        </a>
                      );
                    })()}
                  </div>
                )}
                {updateInfo && updateInfo.status === "latest" && (
                  <p className="updateOk">已是最新版本（{updateInfo.current}）</p>
                )}
                {updateInfo && updateInfo.status === "error" && (
                  <p className="updateErr">检查失败：{updateInfo.notes}</p>
                )}
              </div>
            </section>
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>数据备份与迁移</h2>
                  <p>换机时先导出备份，再到新机器“恢复备份”即可迁移全部历史</p>
                </div>
              </div>
              <div className="form">
                <div className="formActions">
                  <button onClick={backupDb}><Download size={16} />备份数据</button>
                  <button onClick={restoreDb}>恢复备份…</button>
                </div>
              </div>
            </section>
            <section className="panel">
              <div className="panelTitle">
                <div>
                  <h2>运行日志</h2>
                  <p>最近的后台同步、导出与备份事件</p>
                </div>
              </div>
              <div className="logBox">
                {logLines.length === 0 && <p className="empty">暂无日志</p>}
                {logLines.map((line, i) => (
                  <div className="logLine" key={i}>{line}</div>
                ))}
              </div>
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

/** 活跃时长趋势柱状图：按传入的聚合结果绘制（日或周） */
type TrendBar = { label: string; minutes: number; date: string };

function MiniBarChart({ bars, target }: { bars: TrendBar[]; target?: number }) {
  const max = Math.max(...bars.map((b) => b.minutes), target || 60, 60);
  const W = Math.max(340, bars.length * 42);
  const H = 172;
  const padB = 28;
  const padT = 22;
  const chartH = H - padB - padT;
  const bw = W / bars.length;
  const targetY =
    target && target > 0 ? padT + chartH * (1 - Math.min(target / max, 1)) : null;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="活跃时长趋势柱状图">
      <defs>
        <linearGradient id="grad-bar-mini" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#74c79b" />
          <stop offset="55%" stopColor="#3f9a6e" />
          <stop offset="100%" stopColor="#2c7d57" />
        </linearGradient>
      </defs>
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
      <line x1={2} x2={W - 2} y1={padT + chartH} y2={padT + chartH} className="axisBase" />
      {targetY !== null && (
        <g>
          <line x1={2} x2={W - 2} y1={targetY} y2={targetY} className="targetLine" />
          <text x={W - 4} y={targetY - 5} textAnchor="end" className="targetLabel">
            目标 {Math.round((target as number) / 60)}h
          </text>
        </g>
      )}
      {bars.map((b, i) => {
        const absent = b.minutes === 0;
        const h = Math.max((b.minutes / max) * chartH, 6);
        const x = i * bw + bw * 0.16;
        const w = bw * 0.68;
        const y = padT + chartH - h;
        const label = b.minutes >= 600 ? (b.minutes / 60).toFixed(0) : (b.minutes / 60).toFixed(1);
        const labelText = absent ? "缺勤" : `${label}h`;
        const hit = !absent && target && target > 0 && b.minutes >= target;
        return (
          <g key={b.date} className="barGroup">
            <title>{`${b.date}　活跃 ${formatMinutes(b.minutes)}${absent ? "（无活跃记录）" : ""}${target && target > 0 && !absent ? (hit ? " · 已达标" : " · 未达标") : ""}`}</title>
            <rect
              x={x}
              y={y}
              width={w}
              height={h}
              rx={6}
              className={absent ? "bar barAbsent" : hit ? "bar barHit" : "bar"}
              fill={absent ? "none" : "url(#grad-bar-mini)"}
              style={{ animationDelay: `${i * 0.04}s` }}
            />
            <text x={x + w / 2} y={y - 7} textAnchor="middle" className="barLabel">
              {labelText}
            </text>
            <text x={x + w / 2} y={H - 9} textAnchor="middle" className="barDate">
              {b.label}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

/** 将按日期降序的明细按 7 天一组聚合为周趋势 */
function aggregateByWeek(records: DailyRecord[]): TrendBar[] {
  const sorted = [...records].sort((a, b) => a.date.localeCompare(b.date));
  const weeks: TrendBar[] = [];
  for (let i = 0; i < sorted.length; i += 7) {
    const group = sorted.slice(i, i + 7);
    const minutes = group.reduce((s, d) => s + d.active_minutes, 0);
    weeks.push({ label: group[0].date.slice(5), minutes, date: group[0].date });
  }
  return weeks;
}

/** 作息对比：每天的实际活跃区间 vs 计划工作时段 */
function ScheduleStrip({ days, plan }: { days: DailyRecord[]; plan: Settings }) {
  const rows = days.slice(0, 5);
  const W = 340;
  const rowH = 46;
  const H = rows.length * rowH + 18;
  const padL = 56;
  const padR = 14;
  const x = (sec: number) => padL + (sec / 86400) * (W - padL - padR);

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="作息对比图">
      <defs>
        <linearGradient id="grad-actual" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#5fbf8e" />
          <stop offset="100%" stopColor="#2c7d57" />
        </linearGradient>
      </defs>
      {rows.map((d, i) => {
        const y = i * rowH + 8;
        const rh = 30;
        const planA = Math.max(x(parseHM(plan.lunch_start)) - x(parseHM(plan.work_start)), 0);
        const planB = Math.max(x(parseHM(plan.work_end)) - x(parseHM(plan.lunch_end)), 0);
        const start = d.first_activity_at ? parseHM(d.first_activity_at) : 0;
        const end = d.last_activity_at ? parseHM(d.last_activity_at) : 0;
        return (
          <g key={d.date}>
            <title>{`${d.date}　实际 ${d.first_activity_at ?? "--:--"}–${d.last_activity_at ?? "--:--"}`}</title>
            <rect x={x(parseHM(plan.work_start))} y={y} width={planA} height={rh} rx={7} className="planA" />
            <rect x={x(parseHM(plan.lunch_end))} y={y} width={planB} height={rh} rx={7} className="planB" />
            {end > start && (
              <rect
                x={x(start)}
                y={y + 7}
                width={Math.max(x(end) - x(start), 3)}
                height={rh - 14}
                rx={7}
                className="actualBand"
                fill="url(#grad-actual)"
              />
            )}
            <text x={padL - 8} y={y + rh / 2 + 4} textAnchor="end" className="rowDate">
              {d.date.slice(5)}
            </text>
          </g>
        );
      })}
      {[0, 6, 12, 18, 24].map((h) => (
        <text key={h} x={x(h * 3600)} y={H - 4} textAnchor="middle" className="tickLabel">
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
        <defs>
          <linearGradient id="grad-donut-active" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stopColor="#5fbf8e" />
            <stop offset="100%" stopColor="#236a49" />
          </linearGradient>
          <linearGradient id="grad-donut-idle" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stopColor="#f4b45e" />
            <stop offset="100%" stopColor="#d97f24" />
          </linearGradient>
        </defs>
        <g className="ringReveal">
          <circle cx={70} cy={70} r={R} className="donutTrack" />
          <circle
            cx={70}
            cy={70}
            r={R}
            className="donutActive"
            stroke="url(#grad-donut-active)"
            strokeDasharray={`${dash} ${C - dash}`}
            transform="rotate(-90 70 70)"
          />
          <circle
            cx={70}
            cy={70}
            r={R}
            className="donutIdle"
            stroke="url(#grad-donut-idle)"
            strokeDasharray={`${C - dash} ${dash}`}
            transform={`rotate(${-90 + (pct / 100) * 360} 70 70)`}
          />
        </g>
        <text x={70} y={64} textAnchor="middle" className="donutPct">{pct}%</text>
        <text x={70} y={84} textAnchor="middle" className="donutLabel">活跃占比</text>
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
      <defs>
        <linearGradient id="grad-ring" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0%" stopColor="#6cc79a" />
          <stop offset="100%" stopColor="#236a49" />
        </linearGradient>
      </defs>
      <g className="ringReveal">
        <circle cx={80} cy={80} r={R} className="ringTrack" />
        <circle
          cx={80}
          cy={80}
          r={R}
          className="ringActive"
          stroke="url(#grad-ring)"
          strokeDasharray={`${dash} ${C - dash}`}
          transform="rotate(-90 80 80)"
        />
        <text x={80} y={76} textAnchor="middle" className="ringPct">{pct}%</text>
        <text x={80} y={100} textAnchor="middle" className="ringLabel">今日活跃</text>
      </g>
    </svg>
  );
}

function fmtClock(sec: number) {
  const h = Math.floor(sec / 3600) % 24;
  const m = Math.floor((sec % 3600) / 60);
  return `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}`;
}

/* ---------------- 独立数据分析页 ---------------- */
function AnalyticsPage({
  records,
  settings,
  reportRef,
  onExport,
  onExportCsv,
  onExportPdf,
  exportState,
  exportPath,
}: {
  records: DailyRecord[];
  settings: Settings;
  reportRef: React.RefObject<HTMLDivElement>;
  onExport: () => void;
  onExportCsv: () => void;
  onExportPdf: () => void;
  exportState: "idle" | "exporting" | "done" | "error";
  exportPath: string;
}) {
  const [range, setRange] = useState(7);
  const [hours, setHours] = useState<HourBucket[]>([]);
  const data = records.length ? records : fallbackRecords;
  const slice = data.slice(0, range);
  const days = slice.length;
  const useWeeks = days > 14;
  const target = settings.daily_target_minutes;
  const trendBars = useWeeks
    ? aggregateByWeek(slice)
    : [...slice]
        .sort((a, b) => a.date.localeCompare(b.date))
        .map((d) => ({ label: d.date.slice(5), minutes: d.active_minutes, date: d.date }));

  useEffect(() => {
    invoke<HourBucket[]>("hourly_activity", { rangeDays: range === 90 ? 90 : range })
      .then(setHours)
      .catch(() => setHours([]));
  }, [range]);

  const totalActive = slice.reduce((s, d) => s + d.active_minutes, 0);
  const totalIdle = slice.reduce((s, d) => s + d.idle_minutes, 0);
  const avgActive = days ? Math.round(totalActive / days) : 0;
  const avgIdle = days ? Math.round(totalIdle / days) : 0;
  const eff = avgActive + avgIdle > 0 ? Math.round((avgActive / (avgActive + avgIdle)) * 100) : 0;

  const isWeekend = (date: string) => {
    const g = new Date(`${date}T00:00:00`).getDay();
    return g === 0 || g === 6;
  };
  const workdays = slice.filter((d) => !isWeekend(d.date));
  const starts = workdays
    .filter((d) => d.first_activity_at)
    .map((d) => parseHM(d.first_activity_at as string));
  const ends = workdays
    .filter((d) => d.last_activity_at)
    .map((d) => parseHM(d.last_activity_at as string));
  const avgStart = starts.length ? Math.round(starts.reduce((a, b) => a + b, 0) / starts.length) : 0;
  const avgEnd = ends.length ? Math.round(ends.reduce((a, b) => a + b, 0) / ends.length) : 0;

  const tolerance = 45 * 60;
  const ws = parseHM(settings.work_start);
  const we = parseHM(settings.work_end);
  const achieved = workdays.filter((d) => {
    if (!d.first_activity_at || !d.last_activity_at) return false;
    return (
      parseHM(d.first_activity_at) <= ws + tolerance &&
      parseHM(d.last_activity_at) >= we - tolerance
    );
  }).length;
  const achieveRate = workdays.length ? Math.round((achieved / workdays.length) * 100) : 0;

  // 本周 vs 上周 环比：最近 7 天 vs 之前 7 天
  const sortedAll = [...data].sort((a, b) => a.date.localeCompare(b.date));
  const nAll = sortedAll.length;
  const thisWeek = sortedAll.slice(Math.max(0, nAll - 7));
  const lastWeek = sortedAll.slice(Math.max(0, nAll - 14), Math.max(0, nAll - 7));
  const sumActive = (arr: DailyRecord[]) => arr.reduce((s, d) => s + d.active_minutes, 0);
  const sumIdle = (arr: DailyRecord[]) => arr.reduce((s, d) => s + d.idle_minutes, 0);
  const effOf = (arr: DailyRecord[]) => {
    const a = sumActive(arr);
    const i = sumIdle(arr);
    return a + i > 0 ? Math.round((a / (a + i)) * 100) : 0;
  };
  const wsT = parseHM(settings.work_start);
  const weT = parseHM(settings.work_end);
  const achieveOf = (arr: DailyRecord[]) => {
    const wd = arr.filter((d) => {
      const g = new Date(`${d.date}T00:00:00`).getDay();
      return g !== 0 && g !== 6;
    });
    if (!wd.length) return null;
    const hit = wd.filter(
      (d) =>
        d.first_activity_at &&
        d.last_activity_at &&
        parseHM(d.first_activity_at) <= wsT + tolerance &&
        parseHM(d.last_activity_at) >= weT - tolerance
    ).length;
    return Math.round((hit / wd.length) * 100);
  };
  const cmp = (cur: number | null, prev: number | null) =>
    prev === null || cur === null ? null : cur - prev;
  const cActive = cmp(sumActive(thisWeek), sumActive(lastWeek));
  const cEff = cmp(effOf(thisWeek), effOf(lastWeek));
  const cAch = cmp(achieveOf(thisWeek), achieveOf(lastWeek));

  const ranges = [
    { label: "近 7 天", value: 7 },
    { label: "近 30 天", value: 30 },
    { label: "全部", value: 90 },
  ];

  const currentRange = ranges.find((r) => r.value === range)?.label ?? "";
  const generatedAt = new Date().toLocaleDateString("zh-CN");

  return (
    <div className="analytics">
      <div className="exportBar">
        <button className="exportBtn" onClick={onExport} disabled={exportState === "exporting"}>
          <Download size={15} />
          {exportState === "exporting" ? "生成中…" : "导出周报图片"}
        </button>
        <button className="exportBtn" onClick={onExportCsv}>
          <Download size={15} />导出 CSV
        </button>
        <button className="exportBtn" onClick={onExportPdf} disabled={exportState === "exporting"}>
          <Download size={15} />
          {exportState === "exporting" ? "生成中…" : "导出 PDF"}
        </button>
        {exportState === "done" && <span className="exportOk">已保存：{exportPath}</span>}
        {exportState === "error" && <span className="exportErr">导出失败，请重试</span>}
      </div>
      <div className="reportSheet" ref={reportRef}>
        <div className="reportHead">
          <div>
            <h2>数据分析报告</h2>
            <p>统计区间：{currentRange} · 生成于 {generatedAt}</p>
          </div>
          <div className="reportBrand">Activity Worklog</div>
        </div>
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
          <span>工作日达成率</span>
          <strong>{workdays.length ? `${achieveRate}%` : "—"}</strong>
        </div>
      </section>

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>本周 vs 上周</h2>
            <p>最近 7 天 vs 前 7 天（活跃总时长 / 效率 / 工作日达成率）</p>
          </div>
        </div>
        <div className="kpiGrid">
          <CompareItem label="活跃总时长" cur={sumActive(thisWeek)} prev={sumActive(lastWeek)} unit="min" />
          <CompareItem label="平均效率" cur={effOf(thisWeek)} prev={effOf(lastWeek)} unit="pct" />
          <CompareItem label="工作日达成率" cur={achieveOf(thisWeek)} prev={achieveOf(lastWeek)} unit="pct" />
        </div>
      </section>

      {days < 3 && (
        <p className="hint">累计记录天数较少，多用几天后统计会更准确。</p>
      )}

      {days >= 3 && workdays.length === 0 && (
        <p className="hint">该区间没有工作日（周一至周五）记录，工作日达成率暂不计算。</p>
      )}

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>活跃时长趋势</h2>
            <p>{useWeeks ? `最近 ${days} 天 · 按周聚合（每周活跃总时长）` : `最近 ${days} 天每日活跃时长`}</p>
          </div>
        </div>
        <div className="charts">
          <div className="chartCard wide">
            <MiniBarChart bars={trendBars} target={target || undefined} />
            <ChartLegend />
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
          <ChartLegend />
        </div>
      </section>

      <section className="panel">
        <div className="panelTitle">
          <div>
            <h2>活跃时段热力图</h2>
            <p>一天内每小时的活跃分布（{useWeeks ? `近 ${days} 天` : `最近 ${days} 天`}累计）</p>
          </div>
        </div>
        <div className="chartCard">
          <HourHeatmap hours={hours} />
        </div>
      </section>
      </div>
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
  const H = 176;
  const padB = 28;
  const padT = 22;
  const chartH = H - padB - padT;
  const bw = W / 7;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="星期分布柱状图">
      <defs>
        <linearGradient id="grad-bar-week" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#74c79b" />
          <stop offset="100%" stopColor="#2c7d57" />
        </linearGradient>
        <linearGradient id="grad-bar-weekend" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#f6bd6e" />
          <stop offset="100%" stopColor="#e0852b" />
        </linearGradient>
      </defs>
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
      <line x1={2} x2={W - 2} y1={padT + chartH} y2={padT + chartH} className="axisBase" />
      {avg.map((v, i) => {
        const h = Math.max((v / max) * chartH, 3);
        const x = i * bw + bw * 0.18;
        const w = bw * 0.64;
        const y = padT + chartH - h;
        const isWeekend = i === 0 || i === 6;
        return (
          <g key={i} className="barGroup">
            <title>{`${names[i]}　日均 ${(v / 60).toFixed(1)} 小时`}</title>
            <rect
              x={x}
              y={y}
              width={w}
              height={h}
              rx={6}
              className={isWeekend ? "barWeekend" : "bar"}
              fill={isWeekend ? "url(#grad-bar-weekend)" : "url(#grad-bar-week)"}
              style={{ animationDelay: `${i * 0.05}s` }}
            />
            <text x={x + w / 2} y={y - 7} textAnchor="middle" className="barLabel">
              {(v / 60).toFixed(1)}
            </text>
            <text x={x + w / 2} y={H - 9} textAnchor="middle" className="barDate">
              {names[i]}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

/** 本周 vs 上周 对比单项 */
function CompareItem({
  label,
  cur,
  prev,
  unit,
}: {
  label: string;
  cur: number | null;
  prev: number | null;
  unit: "min" | "pct";
}) {
  if (cur === null || prev === null) {
    return (
      <div className="kpi">
        <span>{label}</span>
        <strong>—</strong>
        <span className="delta flat">—</span>
      </div>
    );
  }
  const delta = cur - prev;
  const dir = delta > 0 ? "up" : delta < 0 ? "down" : "flat";
  const arrow = dir === "up" ? "▲" : dir === "down" ? "▼" : "—";
  const compact = (m: number) => {
    const mm = Math.abs(m);
    const h = Math.floor(mm / 60);
    const m2 = mm % 60;
    return h > 0 ? `${h}小时${m2}分` : `${m2}分钟`;
  };
  const deltaText =
    dir === "flat"
      ? "持平"
      : `${delta > 0 ? "+" : "-"}${unit === "min" ? compact(delta) : `${Math.abs(delta)}%`}`;
  const display = unit === "min" ? formatMinutes(cur) : `${cur}%`;
  return (
    <div className="kpi">
      <span>{label}</span>
      <strong>{display}</strong>
      <span className={`delta ${dir}`}>
        {arrow} {deltaText} <em>环比</em>
      </span>
    </div>
  );
}

/** 图表颜色图例（绿=活跃/工作，橙=空闲/离开） */
function ChartLegend() {
  return (
    <div className="chartLegend">
      <span><i className="work" />活跃 / 工作</span>
      <span><i className="away" />空闲 / 离开</span>
    </div>
  );
}

/** 一天内活跃时段热力图（24 小时） */
function HourHeatmap({ hours }: { hours: HourBucket[] }) {
  const max = Math.max(...hours.map((h) => h.minutes), 30);
  const cells = Array.from({ length: 24 }, (_, h) => {
    const found = hours.find((x) => x.hour === h);
    return found ? found.minutes : 0;
  });
  return (
    <div className="heatmap">
      {cells.map((v, h) => {
        const intensity = max > 0 ? v / max : 0;
        return (
          <div
            key={h}
            className="heatCell"
            title={`${String(h).padStart(2, "0")}:00　活跃 ${v} 分钟`}
            style={{
              background: `rgba(44, 125, 87, ${0.12 + intensity * 0.88})`,
              color: intensity > 0.45 ? "#fff" : "var(--text-dim)",
            }}
          >
            <span className="heatHour">{h}</span>
            {v > 0 && <span className="heatVal">{v}</span>}
          </div>
        );
      })}
      <div className="heatScale">
        <span>低</span>
        <i className="scaleLow" />
        <i className="scaleHigh" />
        <span>高</span>
      </div>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
