import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: true,
  workers: 2,
  timeout: 5_000,
  globalTimeout: 25_000,
  expect: { timeout: 2_000 },
  use: { browserName: "chromium", baseURL: "http://127.0.0.1:1420" },
  webServer: {
    command: "npm run dev -- --host 127.0.0.1",
    url: "http://127.0.0.1:1420",
    timeout: 10_000,
    reuseExistingServer: false,
  },
});
