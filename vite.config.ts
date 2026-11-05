import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 的 devUrl 固定指向 127.0.0.1:14200：
// 端口被占用时必须直接报错（strictPort），否则 Vite 顺延端口会让桌面窗口加载到旧服务或空白。
// 用 14200 而不是 Tauri 惯例的 1420：本机（Windows 开发机）1420 被系统服务长期占用。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 14200,
    strictPort: true
  }
});
