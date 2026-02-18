import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 3001,
    proxy: {
      "/ws/metrics": {
        target: "http://127.0.0.1:9090",
        ws: true,
      },
      "/api": {
        target: "http://127.0.0.1:9090",
      },
      "/control": {
        target: "http://127.0.0.1:9091",
      },
      "/status": {
        target: "http://127.0.0.1:9091",
      },
    },
  },
});
