import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
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

type Settings = {
  idle_threshold_minutes: number;
  autostart_enabled: boolean;
  sync_enabled: boolean;
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

function formatMinutes(minutes: number) {
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return `${h}小时${m.toString().padStart(2, "0")}分`;
}

function App() {
  const [records, setRecords] = useState<DailyRecord[]>([]);
  const [settings, setSettings] = useState<Settings>({
    idle_threshold_minutes: 5,
    autostart_enabled: false,
    sync_enabled: false
  });
  const [status, setStatus] = useState("正在读取本地记录");

  async function refresh() {
    try {
      const [loadedRecords, loadedSettings] = await Promise.all([
        invoke<DailyRecord[]>("list_daily_records"),
        invoke<Settings>("get_settings")
      ]);
      setRecords(loadedRecords);
      setSettings(loadedSettings);
      setStatus("后台监听已开启");
    } catch {
      setRecords(fallbackRecords);
      setStatus("预览模式：运行 Tauri 后连接本地服务");
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  const today = records[0] ?? fallbackRecords[0];
  const totalDays = records.length || fallbackRecords.length;

  const cards = useMemo(
    () => [
      { label: "今日开始", value: today.first_activity_at ?? "--:--", icon: Clock3 },
      { label: "今日结束", value: today.last_activity_at ?? "--:--", icon: Moon },
      { label: "活跃时长", value: formatMinutes(today.active_minutes), icon: Activity },
      { label: "休眠阈值", value: `${settings.idle_threshold_minutes}分钟`, icon: ShieldCheck }
    ],
    [today, settings]
  );

  async function updateSettings(next: Partial<Settings>) {
    const merged = { ...settings, ...next };
    setSettings(merged);
    try {
      await invoke("save_settings", { settings: merged });
      setStatus("设置已保存");
    } catch {
      setStatus("预览模式：设置将在桌面端保存");
    }
  }

  async function exportCsv() {
    try {
      await invoke("export_records_csv");
      setStatus("CSV 已导出到本地数据目录");
    } catch {
      setStatus("预览模式：桌面端可导出 CSV");
    }
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
          <button className="navActive"><Clock3 size={18} />每日记录</button>
          <button><Database size={18} />数据同步</button>
          <button><ShieldCheck size={18} />隐私设置</button>
        </nav>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p>{status}</p>
            <h1>自动统计上下班与工作时段</h1>
          </div>
          <div className="actions">
            <button title="刷新" onClick={refresh}><RefreshCcw size={18} /></button>
            <button title="导出 CSV" onClick={exportCsv}><Download size={18} /></button>
          </div>
        </header>

        <section className="metrics">
          {cards.map((card) => {
            const Icon = card.icon;
            return (
              <article className="metric" key={card.label}>
                <Icon size={19} />
                <span>{card.label}</span>
                <strong>{card.value}</strong>
              </article>
            );
          })}
        </section>

        <section className="panel">
          <div className="panelTitle">
            <div>
              <h2>历史记录</h2>
              <p>共 {totalDays} 天，本地保存，可按需同步数据库</p>
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
          <label className="switch">
            <span><Database size={17} />同步数据库</span>
            <input
              type="checkbox"
              checked={settings.sync_enabled}
              onChange={(event) => updateSettings({ sync_enabled: event.target.checked })}
            />
          </label>
        </section>
      </section>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
