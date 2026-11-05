import React, { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import {
  Activity,
  BarChart3,
  ChevronRight,
  Clock3,
  Database,
  Download,
  Moon,
  Pause,
  Play,
  Power,
  Trash2,
  RefreshCcw,
  SlidersHorizontal,
  Target
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

type Page = "records" | "analytics" | "sync" | "settings";

type SyncSummary = {
  pending: number;
  synced: number;
  last_synced_at: string | null;
};

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

/** 是否运行在 Tauri 桌面容器内（纯浏览器打开时为 false，用于界面预览） */
const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function dateKey(day: Date) {
  const m = String(day.getMonth() + 1).padStart(2, "0");
  const d = String(day.getDate()).padStart(2, "0");
  return `${day.getFullYear()}-${m}-${d}`;
}

/** 分钟数转 HH:MM（超出一天时截断，避免预览数据越界） */
function clockOf(minutes: number) {
  const m = Math.max(0, Math.min(Math.round(minutes), 24 * 60 - 1));
  return `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
}

/** 预览用演示数据：仅在浏览器中打开（无 Tauri 运行时）时使用，不影响桌面端真实记录 */
function buildDemoRecords(days = 28): DailyRecord[] {
  const list: DailyRecord[] = [];
  const today = new Date();
  for (let i = 0; i < days; i += 1) {
    const day = new Date(today.getFullYear(), today.getMonth(), today.getDate() - i);
    const weekend = day.getDay() === 0 || day.getDay() === 6;
    const seed = (day.getDate() * 31 + (day.getMonth() + 1) * 17 + i * 7) % 97;
    if (weekend && seed % 4 === 0) {
      list.push({
        date: dateKey(day),
        first_activity_at: null,
        last_activity_at: null,
        active_minutes: 0,
        idle_minutes: 0,
        sync_status: "pending"
      });
      continue;
    }
    const start = weekend ? 9 * 60 + 40 + (seed % 110) : 9 * 60 + 5 + (seed % 26);
    const active = weekend ? 70 + (seed % 150) : 405 + (seed % 85);
    const idle = weekend ? 60 + (seed % 120) : 45 + (seed % 90);
    list.push({
      date: dateKey(day),
      first_activity_at: clockOf(start),
      last_activity_at: clockOf(start + active + idle),
      active_minutes: active,
      idle_minutes: idle,
      sync_status: seed % 5 === 0 ? "pending" : "synced"
    });
  }
  return list;
}

/** 由当天记录切出“工作 / 离开 / 工作”三段，供预览时间线使用 */
function buildDemoSegments(record?: DailyRecord): Segment[] {
  if (!record?.first_activity_at || !record.last_activity_at) return [];
  const [sh, sm] = record.first_activity_at.split(":").map(Number);
  const [eh, em] = record.last_activity_at.split(":").map(Number);
  const start = sh * 60 + sm;
  const end = eh * 60 + em;
  if (end <= start) return [];
  const span = end - start;
  const breakStart = start + Math.round(span * 0.42);
  const breakEnd = start + Math.round(span * 0.54);
  const segment = (from: number, to: number, type: "work" | "away"): Segment => ({
    date: record.date,
    started_at: `${clockOf(from)}:00`,
    ended_at: `${clockOf(to)}:00`,
    segment_type: type
  });
  return [
    segment(start, breakStart, "work"),
    segment(breakStart, breakEnd, "away"),
    segment(breakEnd, end, "work")
  ];
}

/** 预览用的小时活跃分布：贴近真实的上班作息曲线 */
function buildDemoHours(): HourBucket[] {
  const weight = (hour: number) => {
    if (hour < 8 || hour > 21) return 0;
    if (hour === 8) return 0.3;
    if (hour <= 11) return 1;
    if (hour === 12) return 0.22;
    if (hour === 13) return 0.55;
    if (hour <= 17) return 0.92;
    if (hour === 18) return 0.6;
    return 0.18;
  };
  return Array.from({ length: 24 }, (_, hour) => ({
    hour,
    minutes: Math.round(weight(hour) * 54)
  }));
}

const fallbackRecords = buildDemoRecords();
const fallbackSegments = buildDemoSegments(fallbackRecords[0]);

function formatSyncTime(value: string) {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return parsed.toLocaleString("zh-CN", {
    hour12: false,
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit"
  });
}

function formatMinutes(minutes: number) {
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return `${h}小时${m.toString().padStart(2, "0")}分`;
}

/** 简短时长：不足 1 小时只显示分钟，避免「0小时24分」这种读法 */
function formatSpan(minutes: number) {
  const total = Math.max(Math.round(minutes), 0);
  const h = Math.floor(total / 60);
  const m = total % 60;
  if (h === 0) return `${m}分钟`;
  return m === 0 ? `${h}小时` : `${h}小时${m}分`;
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
  const [isPreview, setIsPreview] = useState(false);
  const [saveFeedback, setSaveFeedback] = useState<{
    state: "saving" | "saved" | "error" | "preview";
    message: string;
  } | null>(null);
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
  const [paused, setPaused] = useState(false);
  const [syncInfo, setSyncInfo] = useState<SyncSummary | null>(null);
  const [manualSyncing, setManualSyncing] = useState(false);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [monthFilter, setMonthFilter] = useState("");
  const [expandedDate, setExpandedDate] = useState<string | null>(null);
  const [expandedSegments, setExpandedSegments] = useState<Segment[]>([]);
  const [expandedLoading, setExpandedLoading] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);

  const settingsRef = useRef(settings);
  settingsRef.current = settings;

  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  const recordsRef = useRef(records);
  recordsRef.current = records;
  const saveRequestRef = useRef(0);
  const saveFeedbackTimerRef = useRef<number | null>(null);

  const [exportState, setExportState] = useState<"idle" | "exporting" | "done" | "error">("idle");
  const [exportPath, setExportPath] = useState("");
  const reportRef = useRef<HTMLDivElement>(null);

  const [logLines, setLogLines] = useState<string[]>([]);
  const pushLog = useCallback((msg: string) => {
    const ts = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    setLogLines((prev) => [`${ts}  ${msg}`, ...prev].slice(0, 60));
  }, []);


  const displayRecords = records.length ? records : isPreview ? fallbackRecords : [];
  const months = useMemo(() => {
    const set = new Set<string>();
    for (const record of displayRecords) set.add(record.date.slice(0, 7));
    return Array.from(set);
  }, [displayRecords]);
  const visibleRecords = monthFilter
    ? displayRecords.filter((record) => record.date.startsWith(monthFilter))
    : displayRecords;
  const today = displayRecords[0] ?? null;
  const todayStr = today?.date ?? "";

  const syncState = syncTestResult
    ? /^(连接成功|已同步|没有待同步)/.test(syncTestResult)
      ? "ok"
      : "err"
    : "";

  const refresh = useCallback(async () => {
    try {
      const [loadedRecords, loadedSettings, loadedSync] = await Promise.all([
        invoke<DailyRecord[]>("list_daily_records"),
        invoke<Settings>("get_settings"),
        invoke<SyncSummary>("sync_summary").catch(() => null)
      ]);
      setRecords(loadedRecords);
      setSettings(loadedSettings);
      setSyncInfo(loadedSync);
      setIsPreview(false);
      setStatus(pausedRef.current ? "记录已暂停" : "后台监听已开启");
      return loadedSettings;
    } catch {
      setIsPreview(true);
      if (inTauri) {
        setRecords([]);
        setSegments([]);
        setStatus("本地数据读取失败，请重启应用");
      } else {
        // 浏览器预览：没有本地服务，用演示数据渲染完整界面
        setRecords(fallbackRecords);
        setSettings({ ...defaultSettings, onboarded: true });
        setSyncInfo({
          pending: 2,
          synced: 26,
          last_synced_at: new Date(Date.now() - 36e5).toISOString()
        });
        setStatus("预览模式：未连接本地服务，展示演示数据");
      }
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

  // 暂停状态可能在托盘里被改，窗口重新获得焦点时同步一次
  useEffect(() => {
    const syncPaused = () => {
      invoke<boolean>("is_paused")
        .then(setPaused)
        .catch(() => {});
    };
    syncPaused();
    window.addEventListener("focus", syncPaused);
    return () => window.removeEventListener("focus", syncPaused);
  }, []);

  // 采集失败/恢复：macOS 未授权「输入监控」时最常见，必须让用户看见
  useEffect(() => {
    let offError: (() => void) | null = null;
    let offOk: (() => void) | null = null;
    (async () => {
      try {
        offError = await listen<string>("capture-error", (event) => {
          setStatus("无法读取系统活动状态，请检查辅助功能 / 输入监控权限");
          pushLog(`采集失败：${event.payload}`);
        });
        offOk = await listen("capture-ok", () => {
          setStatus("后台监听已开启");
          pushLog("活动采集已恢复");
        });
      } catch {
        // 预览模式无事件通道
      }
    })();
    return () => {
      offError?.();
      offOk?.();
    };
  }, [pushLog]);

  useEffect(() => {
    if (!todayStr) return;
    invoke<Segment[]>("list_segments", { date: todayStr })
      .then(setSegments)
      .catch(() => setSegments(fallbackSegments));
  }, [todayStr, records]);

  // 键盘快捷键：⌘/Ctrl+R 刷新、⌘/Ctrl+, 打开设置、⌘/Ctrl+1..4 切换页面
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey) return;
      const key = event.key.toLowerCase();
      if (key === "r") {
        event.preventDefault();
        refresh();
      } else if (key === ",") {
        event.preventDefault();
        setPage("settings");
      } else if (key >= "1" && key <= "4") {
        event.preventDefault();
        const target = (["records", "analytics", "sync", "settings"] as Page[])[
          Number(key) - 1
        ];
        if (target) setPage(target);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [refresh]);

  // 窗口尺寸记忆：启动恢复上次尺寸，拖动停止后保存
  useEffect(() => {
    if (!ready || !settings.onboarded) return;
    let win: ReturnType<typeof getCurrentWindow>;
    try {
      win = getCurrentWindow();
    } catch {
      return;
    }
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
          const t = recordsRef.current[0];
          if (!t) {
            setPage("records");
            setStatus("今日还没有活动记录");
            return;
          }
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

  async function toggleDay(date: string) {
    if (expandedDate === date) {
      setExpandedDate(null);
      return;
    }
    setExpandedDate(date);
    setExpandedLoading(true);
    setExpandedSegments([]);
    try {
      const list = await invoke<Segment[]>("list_segments", { date });
      setExpandedSegments(list);
    } catch {
      // 浏览器预览没有本地库，用该天的演示时段填充
      setExpandedSegments(
        inTauri ? [] : buildDemoSegments(displayRecords.find((item) => item.date === date))
      );
    } finally {
      setExpandedLoading(false);
    }
  }

  async function deleteDay(date: string) {
    if (pendingDelete !== date) {
      setPendingDelete(date);
      window.setTimeout(() => setPendingDelete((cur) => (cur === date ? null : cur)), 4000);
      return;
    }
    setPendingDelete(null);
    try {
      await invoke("delete_day", { date });
      pushLog(`已删除 ${date} 的记录`);
      await refresh();
    } catch (err) {
      pushLog(`删除失败：${err}`);
    }
  }

  async function clearAllRecords() {
    if (!confirmClear) {
      setConfirmClear(true);
      window.setTimeout(() => setConfirmClear(false), 5000);
      return;
    }
    setConfirmClear(false);
    try {
      const msg = await invoke<string>("clear_all_records");
      pushLog(msg);
      await refresh();
      setPage("records");
    } catch (err) {
      pushLog(`清除失败：${err}`);
    }
  }

  async function toggleRecording() {
    try {
      const nowPaused = await invoke<boolean>("toggle_pause");
      setPaused(nowPaused);
      setStatus(nowPaused ? "记录已暂停" : "后台监听已开启");
      pushLog(nowPaused ? "已暂停记录" : "已恢复记录");
    } catch {
      setStatus("操作失败，请重试");
    }
  }

  async function syncNow() {
    setManualSyncing(true);
    try {
      const msg = await invoke<string>("sync_now");
      setSyncTestResult(msg);
      pushLog(msg);
      await refresh();
    } catch (err) {
      setSyncTestResult(String(err));
      pushLog(`立即同步失败：${err}`);
    } finally {
      setManualSyncing(false);
    }
  }

  async function updateSettings(next: Partial<Settings>, silent = false) {
    const merged = { ...settingsRef.current, ...next };
    const requestId = ++saveRequestRef.current;
    setSettings(merged);
    if (!silent) {
      if (saveFeedbackTimerRef.current) window.clearTimeout(saveFeedbackTimerRef.current);
      setSaveFeedback({ state: "saving", message: "正在保存设置…" });
    }
    try {
      await invoke("save_settings", { settings: merged });
      if (!silent && requestId === saveRequestRef.current) {
        setStatus("设置已保存");
        setSaveFeedback({ state: "saved", message: "设置已保存" });
        saveFeedbackTimerRef.current = window.setTimeout(() => setSaveFeedback(null), 2200);
      }
    } catch {
      if (!silent && requestId === saveRequestRef.current) {
        const preview = isPreview;
        setStatus(preview ? "预览模式：设置将在桌面端保存" : "设置保存失败，请重试");
        setSaveFeedback({
          state: preview ? "preview" : "error",
          message: preview ? "预览模式下不会写入本地设置" : "保存失败，请重试"
        });
        saveFeedbackTimerRef.current = window.setTimeout(() => setSaveFeedback(null), 3200);
      }
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
        <section className="workspace loading">加载中…</section>
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
            className={page === "settings" ? "navActive" : ""}
            onClick={() => setPage("settings")}
          >
            <SlidersHorizontal size={18} />设置
          </button>
        </nav>
        <div className="sidebarFoot">
          <span className={paused ? "liveDot paused" : "liveDot"} />
          {paused ? "记录已暂停" : "后台监听已开启"}
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p className={paused ? "paused" : undefined}>{status}</p>
            <h1>
              {page === "records" && "每日记录"}
              {page === "analytics" && "数据分析"}
              {page === "sync" && "数据同步"}
              {page === "settings" && "设置"}
            </h1>
          </div>
          {page === "records" && (
            <div className="actions">
              {!isPreview && (
                <button
                  title={paused ? "恢复记录" : "暂停记录"}
                  aria-pressed={paused}
                  className={paused ? "isPaused" : undefined}
                  onClick={toggleRecording}
                >
                  {paused ? <Play size={18} /> : <Pause size={18} />}
                </button>
              )}
              <button title="刷新" onClick={refresh}><RefreshCcw size={18} /></button>
              <button title="导出 CSV" onClick={exportCsv}><Download size={18} /></button>
            </div>
          )}
        </header>

        {page === "records" && !today && (
          <section className="panel firstRecordEmpty">
            <div className="emptyIcon"><Activity size={26} /></div>
            <h2>正在等待首次活动记录</h2>
            <p>保持应用在后台运行，产生键盘或鼠标活动后，这里会自动显示今天的开始时间和工作时段。</p>
            <div className="emptyActions">
              <button className="primary" onClick={refresh}><RefreshCcw size={17} />重新检查</button>
              <button onClick={() => setPage("settings")}><SlidersHorizontal size={17} />检查记录设置</button>
            </div>
          </section>
        )}

        {page === "records" && today && (
          <>
            <div className="todayStatus">
              <div>
                <span className="statusDot" />
                <strong>今日记录正常</strong>
              </div>
              <span>
                {`${isPreview ? "演示数据 · " : ""}${new Date(
                  `${today.date}T00:00:00`
                ).toLocaleDateString("zh-CN", { weekday: "long" })} · 共 ${
                  segments.length
                } 段记录`}
              </span>
            </div>
            <section className="hero">
              <div className="heroRing">
                <TodayRing
                  active={today.active_minutes}
                  idle={today.idle_minutes}
                  target={settings.daily_target_minutes}
                />
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
                  {settings.daily_target_minutes > 0 ? (
                    <>
                      <span>
                        <Target size={16} />
                        {today.active_minutes >= settings.daily_target_minutes
                          ? "超出目标"
                          : "距目标"}
                      </span>
                      <strong>
                        {today.active_minutes >= settings.daily_target_minutes
                          ? `+${formatSpan(today.active_minutes - settings.daily_target_minutes)}`
                          : formatSpan(settings.daily_target_minutes - today.active_minutes)}
                      </strong>
                    </>
                  ) : (
                    <>
                      <span><Target size={16} />活跃占比</span>
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
                    </>
                  )}
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
                <div className="timelineBar">
                  {segments.map((segment, index) => {
                    const startSec = timeToSeconds(segment.started_at);
                    const endSec = timeToSeconds(segment.ended_at);
                    const dur = Math.max(Math.round((endSec - startSec) / 60), 0);
                    const tip = `${segment.segment_type === "work" ? "工作" : "离开"} ${shortTime(
                      segment.started_at
                    )}–${shortTime(segment.ended_at)} · ${dur}分钟`;
                    return (
                      <div
                        key={index}
                        className={`timelineFill ${segment.segment_type}`}
                        data-tip={tip}
                        title={tip}
                        style={{
                          left: `${(startSec / 86400) * 100}%`,
                          width: `${Math.max(((endSec - startSec) / 86400) * 100, 0.25)}%`
                        }}
                      />
                    );
                  })}
                </div>
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
                  <p>共 {displayRecords.length} 天，本地保存，可按需同步数据库</p>
                </div>
                {months.length > 1 && (
                  <select
                    className="monthFilter"
                    value={monthFilter}
                    onChange={(event) => setMonthFilter(event.target.value)}
                    aria-label="按月份筛选"
                  >
                    <option value="">全部月份</option>
                    {months.map((month) => (
                      <option key={month} value={month}>
                        {month.replace("-", " 年 ")} 月
                      </option>
                    ))}
                  </select>
                )}
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
                {visibleRecords.map((record) => {
                  const empty = record.active_minutes + record.idle_minutes === 0;
                  const status =
                    record.sync_status === "synced"
                      ? "已同步"
                      : record.sync_status === "pending"
                      ? "待同步"
                      : record.sync_status === "error"
                      ? "同步失败"
                      : record.sync_status;
                  const open = expandedDate === record.date;
                  return (
                    <Fragment key={record.date}>
                      <div
                        className={`row rowClickable${empty ? " rowEmpty" : ""}${open ? " rowOpen" : ""}`}
                        role="button"
                        tabIndex={0}
                        aria-expanded={open}
                        onClick={() => toggleDay(record.date)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter" || event.key === " ") {
                            event.preventDefault();
                            toggleDay(record.date);
                          }
                        }}
                      >
                        <span className="cellDate">
                          <ChevronRight
                            size={12}
                            className={open ? "rowCaret open" : "rowCaret"}
                          />
                          {record.date}
                        </span>
                        <span>{record.first_activity_at ?? "—"}</span>
                        <span>{record.last_activity_at ?? "—"}</span>
                        <span>{empty ? "—" : formatMinutes(record.active_minutes)}</span>
                        <span>{empty ? "—" : formatMinutes(record.idle_minutes)}</span>
                        <span className={`sync ${record.sync_status}`}>{status}</span>
                        <button
                          type="button"
                          className={`rowDelete${pendingDelete === record.date ? " confirming" : ""}`}
                          title={
                            pendingDelete === record.date
                              ? "再次点击确认删除"
                              : "删除这一天的记录"
                          }
                          onClick={(event) => {
                            event.stopPropagation();
                            deleteDay(record.date);
                          }}
                        >
                          {pendingDelete === record.date ? "确认删除" : <Trash2 size={13} />}
                        </button>
                      </div>
                      {open && (
                        <div className="rowDetail">
                          {expandedLoading && <span className="rowDetailEmpty">读取时段…</span>}
                          {!expandedLoading && expandedSegments.length === 0 && (
                            <span className="rowDetailEmpty">这一天没有时段记录</span>
                          )}
                          {!expandedLoading &&
                            expandedSegments.map((segment, index) => {
                              const minutes = Math.max(
                                Math.round(
                                  (timeToSeconds(segment.ended_at) -
                                    timeToSeconds(segment.started_at)) /
                                    60
                                ),
                                0
                              );
                              return (
                                <div
                                  className={`segmentRow ${segment.segment_type}`}
                                  key={`${segment.started_at}-${index}`}
                                >
                                  <i className="segmentDot" />
                                  <span className="segmentType">
                                    {segment.segment_type === "work" ? "工作" : "离开"}
                                  </span>
                                  <span className="segmentTime">
                                    {shortTime(segment.started_at)} – {shortTime(segment.ended_at)}
                                  </span>
                                  <span className="segmentDur">{formatSpan(minutes)}</span>
                                </div>
                              );
                            })}
                        </div>
                      )}
                    </Fragment>
                  );
                })}
              </div>
            </section>
          </>
        )}

        {page === "analytics" && (
          <AnalyticsPage
            records={displayRecords}
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
                <span><Database size={16} />启用同步</span>
                <input
                  type="checkbox"
                  checked={settings.sync_enabled}
                  onChange={(event) => updateSettings({ sync_enabled: event.target.checked })}
                />
                <span className="switchTrack" aria-hidden="true" />
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
                <button
                  onClick={syncNow}
                  disabled={manualSyncing || !settings.sync_enabled}
                  title={settings.sync_enabled ? "立刻推送待同步记录" : "请先启用同步"}
                >
                  <RefreshCcw size={15} />
                  {manualSyncing ? "同步中…" : "立即同步"}
                </button>
                {syncTestResult && (
                  <span className={`syncResult ${syncState}`}>{syncTestResult}</span>
                )}
              </div>
              {syncInfo && (
                <div className="syncSummary">
                  <span>待同步 <strong>{syncInfo.pending}</strong> 天</span>
                  <span>已同步 <strong>{syncInfo.synced}</strong> 天</span>
                  <span>
                    上次同步{" "}
                    <strong>
                      {syncInfo.last_synced_at ? formatSyncTime(syncInfo.last_synced_at) : "从未"}
                    </strong>
                  </span>
                </div>
              )}
            </div>
          </section>
        )}

        {page === "settings" && (
          <>
            <section className="panel settingsPanel">
              <div className="panelTitle">
                <div>
                  <h2>常用设置</h2>
                  <p>调整记录规则、外观和后台驻留方式</p>
                </div>
              </div>
              <div className="settingsQuickGrid">
                <label className="settingGroup idleControl">
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
                <label className="settingGroup switch">
                  <span><Power size={16} />开机自启</span>
                  <input
                    type="checkbox"
                    checked={settings.autostart_enabled}
                    onChange={(event) => updateSettings({ autostart_enabled: event.target.checked })}
                  />
                  <span className="switchTrack" aria-hidden="true" />
                </label>
                <label className="settingGroup switch">
                  <span title="隐藏 Dock 图标，只在菜单栏常驻（需重启应用生效）">
                    <Power size={16} />菜单栏驻留模式
                  </span>
                  <input
                    type="checkbox"
                    checked={settings.menubar_only}
                    onChange={(event) => {
                      updateSettings({ menubar_only: event.target.checked });
                      pushLog(
                        event.target.checked
                          ? "菜单栏驻留模式已开启，重启应用后生效"
                          : "菜单栏驻留模式已关闭，重启应用后生效"
                      );
                    }}
                  />
                  <span className="switchTrack" aria-hidden="true" />
                </label>
                <div className="settingGroup">
                  <span>外观</span>
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
                  <h2>关于</h2>
                  <p>本地隐私工作记录 · 数据始终保存在本机</p>
                </div>
                <button className="panelBtn" onClick={checkForUpdate} disabled={checking}>
                  <RefreshCcw size={14} />
                  {checking ? "检查中…" : "检查更新"}
                </button>
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
                  <strong>⌘R 刷新 · ⌘, 打开设置 · ⌘1–4 切换页面</strong>
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
                  <button
                    className={`dangerBtn${confirmClear ? " confirming" : ""}`}
                    onClick={clearAllRecords}
                  >
                    <Trash2 size={16} />
                    {confirmClear ? "再次点击清除全部" : "清除全部记录"}
                  </button>
                </div>
                <p className="hint">清除只删除本机的活动记录，设置与同步配置会保留。</p>
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
        {saveFeedback && (
          <div className={`saveToast ${saveFeedback.state}`} role="status" aria-live="polite">
            <span className="toastDot" />
            {saveFeedback.message}
          </div>
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
                <span><Database size={16} />启用同步</span>
                <input
                  type="checkbox"
                  checked={settings.sync_enabled}
                  onChange={(event) => onChange({ sync_enabled: event.target.checked }, true)}
                />
                <span className="switchTrack" aria-hidden="true" />
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
                <span><Power size={16} />开机自启</span>
                <input
                  type="checkbox"
                  checked={settings.autostart_enabled}
                  onChange={(event) => onChange({ autostart_enabled: event.target.checked }, true)}
                />
                <span className="switchTrack" aria-hidden="true" />
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

/** 让手写 SVG 按容器真实宽度 1:1 绘制，避免 viewBox 缩放把图表压小 */
function useChartWidth(fallback = 480) {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => setWidth(Math.round(el.getBoundingClientRect().width));
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width > 0 ? width : fallback] as const;
}

/** 活跃时长趋势柱状图：按传入的聚合结果绘制（日或周） */
type TrendBar = { label: string; minutes: number; date: string };

function TrendChart({ bars, target }: { bars: TrendBar[]; target?: number }) {
  const [wrapRef, W] = useChartWidth(560);
  const max = Math.max(...bars.map((b) => b.minutes), target || 60, 60);
  const H = 210;
  const padB = 26;
  const padT = 34;
  const padL = 26;
  const padR = 26;
  const chartH = H - padB - padT;
  const baseY = padT + chartH;
  const innerW = Math.max(W - padL - padR, 10);
  const step = bars.length > 1 ? innerW / (bars.length - 1) : 0;
  const cx = (i: number) => (bars.length > 1 ? padL + step * i : W / 2);
  const cy = (minutes: number) => padT + chartH * (1 - minutes / max);
  const dense = bars.length > 16;
  const targetY = target && target > 0 ? cy(target) : null;

  const points = bars.map((bar, i) => ({
    ...bar,
    x: cx(i),
    y: cy(bar.minutes),
    absent: bar.minutes === 0
  }));
  const line = points
    .map((p, i) => `${i === 0 ? "M" : "L"}${p.x.toFixed(1)} ${p.y.toFixed(1)}`)
    .join(" ");
  const area =
    points.length > 0
      ? `${line} L${points[points.length - 1].x.toFixed(1)} ${baseY} L${points[0].x.toFixed(1)} ${baseY} Z`
      : "";

  return (
    <div className="chartFit" ref={wrapRef}>
      <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="活跃时长趋势图">
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
        <line x1={0} x2={W} y1={baseY} y2={baseY} className="axisBase" />

        {targetY !== null && (
          <g>
            <line x1={0} x2={W} y1={targetY} y2={targetY} className="targetLine" />
            <text x={W - 2} y={13} textAnchor="end" className="targetLabel">
              目标 {Math.round((target as number) / 60)}h
            </text>
          </g>
        )}

        {area && <path d={area} className="trendArea" />}
        {line && <path d={line} className="trendLine" />}

        {points.map((p, i) => {
          const label = p.minutes >= 600 ? (p.minutes / 60).toFixed(0) : (p.minutes / 60).toFixed(1);
          const hit = !p.absent && target && target > 0 && p.minutes >= target;
          return (
            <g key={p.date} className="barGroup">
              <title>{`${p.date}　活跃 ${formatMinutes(p.minutes)}${p.absent ? "（无活跃记录）" : ""}${target && target > 0 && !p.absent ? (hit ? " · 已达标" : " · 未达标") : ""}`}</title>
              <rect
                x={p.x - Math.max(step, 8) / 2}
                y={padT}
                width={Math.max(step, 8)}
                height={chartH}
                fill="transparent"
              />
              <circle
                cx={p.x}
                cy={p.y}
                r={3.5}
                className={p.absent ? "trendDot absent" : "trendDot"}
              />
              {(!dense || i % 2 === 0) && (
                <text x={p.x} y={p.y - 11} textAnchor="middle" className="barLabel">
                  {p.absent ? "—" : `${label}h`}
                </text>
              )}
              {(!dense || i % 2 === 0) && (
                <text x={p.x} y={H - 8} textAnchor="middle" className="barDate">
                  {p.label}
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </div>
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
  const [wrapRef, W] = useChartWidth(460);
  const rows = days.slice(0, 5);
  const rowH = 42;
  const H = rows.length * rowH + 20;
  const padL = 40;
  const padR = 8;
  const dayStart = 6 * 3600;
  const dayEnd = 22 * 3600;
  const x = (sec: number) =>
    padL + ((Math.min(Math.max(sec, dayStart), dayEnd) - dayStart) / (dayEnd - dayStart)) * (W - padL - padR);

  return (
    <div className="chartFit" ref={wrapRef}>
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="作息对比图">
      <defs>
        <linearGradient id="grad-actual" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#3a9d7c" />
          <stop offset="100%" stopColor="#2f8a6c" />
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
            <rect x={x(parseHM(plan.work_start))} y={y + 6} width={planA} height={rh - 12} rx={5} className="planA" />
            <rect x={x(parseHM(plan.lunch_end))} y={y + 6} width={planB} height={rh - 12} rx={5} className="planB" />
            {end > start && (
              <rect
                x={x(start)}
                y={y + rh / 2 - 5}
                width={Math.max(x(end) - x(start), 3)}
                height={10}
                rx={5}
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
      {[6, 10, 14, 18, 22].map((h) => (
        <text key={h} x={x(h * 3600)} y={H - 6} textAnchor="middle" className="tickLabel">
          {String(h).padStart(2, "0")}:00
        </text>
      ))}
    </svg>
    </div>
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
            <stop offset="0%" stopColor="#4aa887" />
            <stop offset="100%" stopColor="#2f8a6c" />
          </linearGradient>
          <linearGradient id="grad-donut-idle" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stopColor="#d19a4b" />
            <stop offset="100%" stopColor="#bd7f2f" />
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
function TodayRing({
  active,
  idle,
  target
}: {
  active: number;
  idle: number;
  target: number;
}) {
  const total = active + idle;
  const useTarget = target > 0;
  // 有目标时画「目标达成度」，没有目标才退回「活跃占比」
  const ratio = useTarget ? active / target : total > 0 ? active / total : 0;
  const rawPct = Math.round(ratio * 100);
  const arcPct = Math.min(Math.max(ratio, 0), 1);
  const R = 58;
  const C = 2 * Math.PI * R;
  const dash = arcPct * C;
  const targetHours = (target / 60).toFixed(target % 60 === 0 ? 0 : 1);
  return (
    <svg
      viewBox="0 0 160 160"
      className="ring"
      role="img"
      aria-label={useTarget ? "今日目标达成度" : "今日活跃占比"}
    >
      <defs>
        <linearGradient id="grad-ring" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0%" stopColor="#4aa887" />
          <stop offset="100%" stopColor="#2f8a6c" />
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
        <text x={80} y={76} textAnchor="middle" className="ringPct">{rawPct}%</text>
        <text x={80} y={100} textAnchor="middle" className="ringLabel">
          {useTarget ? `目标 ${targetHours}h` : "今日活跃"}
        </text>
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
    invoke<HourBucket[]>("hourly_activity", { rangeDays: range === 365 ? 365 : range })
      .then(setHours)
      .catch(() => setHours(inTauri ? [] : buildDemoHours()));
  }, [range]);

  const totalActive = slice.reduce((s, d) => s + d.active_minutes, 0);
  const totalIdle = slice.reduce((s, d) => s + d.idle_minutes, 0);
  const avgActive = days ? Math.round(totalActive / days) : 0;
  const avgIdle = days ? Math.round(totalIdle / days) : 0;
  const eff = avgActive + avgIdle > 0 ? Math.round((avgActive / (avgActive + avgIdle)) * 100) : 0;

  // 本期小结：把「最早/最晚/达标/早退」这些要自己从图里读的结论直接写出来
  const activeDays = slice.filter((d) => d.active_minutes + d.idle_minutes > 0);
  const earliestStart = activeDays.reduce<string | null>(
    (best, d) =>
      d.first_activity_at && (!best || d.first_activity_at < best) ? d.first_activity_at : best,
    null
  );
  const latestEnd = activeDays.reduce<string | null>(
    (best, d) =>
      d.last_activity_at && (!best || d.last_activity_at > best) ? d.last_activity_at : best,
    null
  );
  // 达标 / 早退只按工作日算，周末加班或早收工不该进这两项
  const activeWorkdays = activeDays.filter((d) => {
    const weekday = new Date(`${d.date}T00:00:00`).getDay();
    return weekday !== 0 && weekday !== 6;
  });
  const overTarget =
    target > 0 ? activeWorkdays.filter((d) => d.active_minutes >= target).length : 0;
  const earlyOff = activeWorkdays.filter(
    (d) => d.last_activity_at && parseHM(d.last_activity_at) < parseHM(settings.work_end)
  ).length;

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
    { label: "全部", value: 365 },
  ];

  const currentRange = ranges.find((r) => r.value === range)?.label ?? "";
  const generatedAt = new Date().toLocaleDateString("zh-CN");

  return (
    <div className="analytics">
      <div className="exportBar">
        <div className="tabs" role="group" aria-label="统计区间">
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
        <div className="exportActions">
          <button
            className="exportBtn primary"
            onClick={onExport}
            disabled={exportState === "exporting"}
          >
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
        </div>
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

        {activeDays.length === 0 ? (
          <p className="insight empty">这段时间还没有活动记录，保持应用在后台运行即可自动统计。</p>
        ) : (
          <div className="insight">
            <p>
              本期 <strong>{activeDays.length}</strong> 天有记录，共活跃{" "}
              <strong>{formatMinutes(totalActive)}</strong>，日均{" "}
              <strong>{formatMinutes(avgActive)}</strong>，平均效率 <strong>{eff}%</strong>。
            </p>
            <p>
              最早 <strong>{earliestStart ?? "—"}</strong> 开始、最晚{" "}
              <strong>{latestEnd ?? "—"}</strong> 结束
              {target > 0 && (
                <>
                  ；<strong>{overTarget}</strong> 个工作日达到 {Math.round(target / 60)} 小时目标
                </>
              )}
              {earlyOff > 0 && (
                <>
                  ，另有 <strong>{earlyOff}</strong> 个工作日 {settings.work_end} 前下班
                </>
              )}
              。
            </p>
          </div>
        )}

      <section className="kpiGrid">
        <div className="kpi">
          <span>总活跃时长</span>
          <strong>{formatMinutes(totalActive)}</strong>
          <Delta value={cActive} unit="min" />
        </div>
        <div className="kpi">
          <span>日均活跃</span>
          <strong>{formatMinutes(avgActive)}</strong>
        </div>
        <div className="kpi">
          <span>平均效率</span>
          <strong>{eff}%</strong>
          <Delta value={cEff} unit="pct" />
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
          <Delta value={cAch} unit="pct" />
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
            <TrendChart bars={trendBars} target={target || undefined} />
            <ChartLegend workLabel="活跃时长" awayLabel="每日目标" />
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
            <div className="chartLegend">
              <span><i className="plan" />计划时段</span>
              <span><i className="actual" />实际活跃</span>
            </div>
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
          <ChartLegend workLabel="工作日" awayLabel="周末" />
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
  const [wrapRef, W] = useChartWidth(360);
  const H = 216;
  const padB = 26;
  const padT = 24;
  const chartH = H - padB - padT;
  const slot = W / 7;
  const barW = Math.min(slot * 0.54, 64);

  return (
    <div className="chartFit" ref={wrapRef}>
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" role="img" aria-label="星期分布柱状图">
      <defs>
        <linearGradient id="grad-bar-week" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#4aa887" />
          <stop offset="100%" stopColor="#2f8a6c" />
        </linearGradient>
        <linearGradient id="grad-bar-weekend" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#d9a55a" />
          <stop offset="100%" stopColor="#bd7f2f" />
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
        const h = v === 0 ? 3 : Math.max((v / max) * chartH, 4);
        const cx = i * slot + slot / 2;
        const x = cx - barW / 2;
        const y = padT + chartH - h;
        const isWeekend = i === 0 || i === 6;
        return (
          <g key={i} className="barGroup">
            <title>{`${names[i]}　日均 ${(v / 60).toFixed(1)} 小时`}</title>
            <rect
              x={x}
              y={y}
              width={barW}
              height={h}
              rx={5}
              className={isWeekend ? "barWeekend" : "bar"}
              fill={isWeekend ? "url(#grad-bar-weekend)" : "url(#grad-bar-week)"}
              style={{ animationDelay: `${i * 0.05}s` }}
            />
            <text x={cx} y={y - 8} textAnchor="middle" className="barLabel">
              {(v / 60).toFixed(1)}
            </text>
            <text x={cx} y={H - 8} textAnchor="middle" className="barDate">
              {names[i]}
            </text>
          </g>
        );
      })}
    </svg>
    </div>
  );
}

/** KPI 内的环比差值：与上周同期对比 */
function Delta({ value, unit }: { value: number | null; unit: "min" | "pct" }) {
  if (value === null) {
    return (
      <span className="delta flat">
        — <em>vs 上周</em>
      </span>
    );
  }
  const dir = value > 0 ? "up" : value < 0 ? "down" : "flat";
  const arrow = dir === "up" ? "▲" : dir === "down" ? "▼" : "—";
  const compact = (minutes: number) => {
    const mm = Math.abs(minutes);
    const h = Math.floor(mm / 60);
    const m = mm % 60;
    return h > 0 ? `${h}小时${m}分` : `${m}分钟`;
  };
  const text =
    dir === "flat"
      ? "持平"
      : `${value > 0 ? "+" : "-"}${unit === "min" ? compact(value) : `${Math.abs(value)}%`}`;
  return (
    <span className={`delta ${dir}`}>
      {arrow} {text} <em>vs 上周</em>
    </span>
  );
}

/** 图表颜色图例（绿=活跃/工作，橙=空闲/离开） */
function ChartLegend({
  workLabel = "活跃 / 工作",
  awayLabel = "空闲 / 离开"
}: {
  workLabel?: string;
  awayLabel?: string;
}) {
  return (
    <div className="chartLegend">
      <span><i className="work" />{workLabel}</span>
      <span><i className="away" />{awayLabel}</span>
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
        const level = Math.min(4, Math.round(intensity * 4));
        const alpha = [0.05, 0.14, 0.28, 0.45, 0.68][level];
        return (
          <div
            key={h}
            className="heatCell"
            data-level={level}
            title={`${String(h).padStart(2, "0")}:00　活跃 ${v} 分钟`}
            style={{
              background: `rgba(31, 111, 92, ${alpha})`,
              color: level >= 3 ? "#ffffff" : "var(--text-dim)",
            }}
          >
            {h % 3 === 0 && <span className="heatHour">{h}</span>}
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

/** 渲染异常兜底：避免白屏，给出最小可恢复路径 */
class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="crash">
        <h2>界面渲染出错了</h2>
        <p>本地数据未受影响，重新加载即可恢复。</p>
        <code>{error.message}</code>
        <button className="primary" onClick={() => window.location.reload()}>
          重新加载
        </button>
      </div>
    );
  }
}

createRoot(document.getElementById("root")!).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>
);
