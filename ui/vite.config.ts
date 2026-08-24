import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Dev-only: proxy API calls to the nodelm daemon so `npm run dev` works
// without rebuilding the embedded dist/ into the Rust binary each time.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://127.0.0.1:11535",
      "/v1": "http://127.0.0.1:11535",
    },
  },
});
