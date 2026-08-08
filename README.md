# Activity Worklog

一款隐私友好的桌面端工作时间记录工具。它通过系统“最后一次输入时间”判断鼠标键盘是否有活动，用于自动统计每天首次活动时间、末次活动时间和工作时段。

## 当前定位

- 当前开发环境：macOS。
- 优先保证 macOS 可运行，同时保留 Windows 平台实现。
- 默认后台静默运行，并最小化到系统托盘。
- 只记录“是否有活动”和活动时间，不记录输入内容、不截图、不读取文件。
- 本地 SQLite 保存记录，支持 CSV 导出。
- 预留数据库同步接口，后续可接入 Web 后台。

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

1. 在 macOS 上安装 Rust/Cargo 并完成首轮编译。
2. 完善自启开关和托盘暂停/恢复记录。
3. 增加多段工作/离开时段表。
4. 接入数据库同步 API。
5. 在 Windows 上做兼容验证和打包。
6. 增加可查看的 Web 后台。
