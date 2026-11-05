# Activity Worklog

一款隐私友好的桌面端工作时间记录工具。它通过系统“最后一次输入时间”判断鼠标键盘是否有活动，用于自动统计每天首次活动时间、末次活动时间和工作时段。

## 当前定位

- 当前开发环境：macOS。
- 优先保证 macOS 可运行，同时保留 Windows 平台实现。
- 默认后台静默运行，并最小化到系统托盘。
- 只记录“是否有活动”和活动时间，不记录输入内容、不截图、不读取文件。
- 本地 SQLite 保存记录，支持 CSV 导出。
- 支持把记录推送到自建服务器（`POST /api/sync/records`）。
- 应用锁：启动、隐藏窗口或托盘锁定后需要密码才能看到记录，可生成一次性恢复码。
- 隐藏入口：macOS 不出现在 Dock / ⌘Tab，Windows 不进任务栏，保留托盘图标作为唯一入口。

## 项目结构

- `src/`：前端界面。
- `src-tauri/`：Tauri + Rust 后台。
- `docs/需求与实现报告.md`：需求、设计、已完成内容和后续路线。

## 常用命令

```bash
npm install
npm run tauri:dev
npm run tauri:build
```

## 生成安装包

macOS：

```bash
cd /Applications/code/ere
npm install
npm run tauri:build
```

生成位置通常是：

```text
src-tauri/target/release/bundle/dmg/
```

Windows：

```powershell
cd C:\你的项目路径\ere
npm install
npm run tauri:build
```

生成位置通常是：

```text
src-tauri\target\release\bundle\
```

源码会保留在项目目录中，安装包只是额外生成的文件。

## macOS 权限说明

macOS 对后台输入活动检测有系统隐私限制。首次运行后，如果系统提示权限，请到：

`系统设置 -> 隐私与安全性 -> 辅助功能 / 输入监控`

允许 Activity Worklog 或开发中的终端/构建产物。当前方案只读取系统空闲时间，不保存按键内容、不截图、不读取文件。

## 后续重点

已完成：macOS 编译与 DMG 打包、开机自启、托盘暂停/恢复、多段工作/离开时段表、同步配置页与推送接口、数据分析与报表导出。

仍需处理：

1. Windows 兼容：备份恢复的文件选择器与系统通知目前只有 macOS 实现。
2. 运行日志落盘：当前日志只在内存保留最近 60 条。
3. 记录时间编辑：目前只能整天删除，不能改某天的上下班时间。
4. 数据保留期：自动清理 N 天之前的记录。
5. 数据加密（SQLCipher）：应用锁只挡界面，数据库本身仍是明文 SQLite。
6. Web 后台完善：`server/` 已有同步接口与报表页（日报/周报/月报），还需账号体系与图表。
7. macOS 签名与公证、Windows CI 构建与自动更新发布。

详细的代码评审与优化清单见 `docs/评审与优化建议.md`。
