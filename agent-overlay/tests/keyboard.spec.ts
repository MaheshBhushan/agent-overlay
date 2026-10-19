import { expect, test, type Page } from "@playwright/test";
import snapshots from "./fixtures/sessions-update.json" with { type: "json" };
import type {} from "./harness";

const card = (page: Page, id: string) => page.locator(`.card[data-session="${id}"]`);
const replay = (page: Page, sessions: typeof snapshots.initial) =>
  page.evaluate(next => window.overlayTest.replay(next), sessions);
const calls = (page: Page, command: string) =>
  page.evaluate(cmd => window.overlayTest.calls.filter(call => call.command === cmd), command);

test.beforeEach(async ({ page }) => {
  page.on("pageerror", error => { throw error; });
  // Load Tauri's official IPC mock before the unchanged application entrypoint.
  // Browser tests cannot focus terminals, close processes, or install hooks.
  await page.route("http://127.0.0.1:1420/", async route => {
    const response = await route.fetch();
    const html = (await response.text()).replace(
      '<script type="module" src="/src/main.ts"',
      '<script type="module" src="/tests/harness.ts"></script>\n    <script type="module" src="/src/main.ts"',
    );
    await route.fulfill({ response, body: html });
  });
  await page.goto("/");
  await expect(page.locator("#btn-sound")).toHaveText("🔊");
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "Toggle session panel" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.locator("#panel")).toBeVisible();
});

test("Tab and Shift+Tab reach cards, lifecycle buttons, and approval choices in order", async ({ page }) => {
  const sequence = ["#btn-kill-all", "#btn-sound", "#btn-settings", "#btn-refresh", "#btn-minimize",
    '.card[data-session="AO-01"]', '.card[data-session="AO-01"] .kill',
    '.card[data-session="AO-02"]', '.card[data-session="AO-02"] .kill',
    '.card[data-session="AO-03"]', '.card[data-session="AO-03"] .kill', ".approve", ".deny"];
  for (const selector of sequence) {
    await page.keyboard.press("Tab");
    await expect(page.locator(selector)).toBeFocused();
    await expect(page.locator(selector)).toHaveCSS("outline-style", "solid");
  }
  for (const selector of sequence.slice(0, -1).reverse()) {
    await page.keyboard.press("Shift+Tab");
    await expect(page.locator(selector)).toBeFocused();
  }
  expect(await calls(page, "kill_session")).toEqual([]);
  expect(await calls(page, "answer_approval")).toEqual([]);
});

test("arrows and Home/End navigate cards without invoking lifecycle commands", async ({ page }) => {
  await page.locator("#btn-minimize").focus();
  await page.keyboard.press("Tab");
  for (const [key, id] of [["ArrowUp", "AO-01"], ["ArrowDown", "AO-02"], ["ArrowDown", "AO-03"],
    ["ArrowDown", "AO-03"], ["Home", "AO-01"], ["End", "AO-03"], ["ArrowUp", "AO-02"]]) {
    await page.keyboard.press(key);
    await expect(card(page, id)).toBeFocused();
  }
  expect(await calls(page, "focus_session")).toEqual([]);
  expect(await calls(page, "mark_seen")).toEqual([]);
});

test("Enter opens the focused session and Space marks finished work seen", async ({ page }) => {
  await card(page, "AO-01").focus();
  await page.keyboard.press("Enter");
  expect(await calls(page, "focus_session")).toEqual([{ command: "focus_session", args: { sessionId: "AO-01" } }]);
  await card(page, "AO-02").focus();
  await page.keyboard.press("Space");
  await expect(card(page, "AO-02")).not.toHaveClass(/finished/);
  await expect(card(page, "AO-02")).toBeFocused();
  expect((await calls(page, "mark_seen")).map(call => call.args.sessionId)).toEqual(["AO-01", "AO-02"]);
  await page.keyboard.press("Space");
  expect(await calls(page, "mark_seen")).toHaveLength(2);
});

for (const choice of ["approve", "deny"] as const) {
  test(`${choice} remains selected through ticks, reordering, and column moves`, async ({ page }) => {
    const button = card(page, "AO-03").locator(`.${choice}`);
    await card(page, "AO-03").focus();
    await page.keyboard.press("Tab"); // Close
    await page.keyboard.press("Tab"); // Approve
    if (choice === "deny") await page.keyboard.press("Tab");
    const original = await button.elementHandle();
    for (const snapshot of [snapshots.tick, snapshots.moved, snapshots.initial]) {
      await replay(page, snapshot);
      await expect(button).toBeFocused();
      expect(await button.evaluate((node, before) => node === before, original)).toBe(true);
    }
    await page.keyboard.press(choice === "approve" ? "Enter" : "Space");
    expect(await calls(page, "answer_approval")).toEqual([{
      command: "answer_approval", args: { requestId: "request-1", sessionId: "AO-03", allow: choice === "approve" },
    }]);
    expect(await calls(page, "focus_session")).toEqual([]);
    expect(await calls(page, "mark_seen")).toEqual([]);
  });
}

test("focus survives card reordering and updates to head and approval metadata", async ({ page }) => {
  const session = card(page, "AO-03");
  await session.locator(".deny").focus();
  await replay(page, snapshots.tick.map(s => s.approval ? { ...s, approval: { ...s.approval, summary: "updated summary" } } : s));
  await expect(session.locator(".deny")).toBeFocused();
  await session.locator(".kill").focus();
  await replay(page, snapshots.tick.map(s => ({ ...s, cwd: `${s.cwd}/renamed` })));
  await expect(session.locator(".kill")).toBeFocused();
  await card(page, "AO-01").focus();
  await replay(page, snapshots.moved);
  await expect(card(page, "AO-01")).toBeFocused();
});

test("new or resolved approvals return focus to the card without choosing a new action", async ({ page }) => {
  await page.locator(".deny").focus();
  await replay(page, snapshots.replaced);
  await expect(card(page, "AO-03")).toBeFocused();
  expect(await calls(page, "answer_approval")).toEqual([]);
  await page.locator(".approve").focus();
  await replay(page, snapshots.tick.map(s => s.session_id === "AO-03" ? { ...s, approval: null, status: "idle" } : s));
  await expect(card(page, "AO-03")).toBeFocused();
  await expect(page.locator(".approve")).toHaveCount(0);
});

test("pending approvals stay disabled through refreshes and re-enable after failure", async ({ page }) => {
  await page.evaluate(() => { window.overlayTest.hold = "answer_approval"; });
  await page.locator(".deny").focus();
  await page.keyboard.press("Enter");
  for (const snapshot of [snapshots.tick, snapshots.moved]) {
    await replay(page, snapshot);
    await expect(card(page, "AO-03")).toBeFocused();
    await expect(page.locator(".deny")).toBeDisabled();
    await expect(page.locator(".approve")).toBeDisabled();
    await expect(page.locator(".approval-error")).toHaveText("denying…");
  }
  expect(await calls(page, "answer_approval")).toHaveLength(1);
  await page.evaluate(() => window.overlayTest.release());
  await replay(page, snapshots.replaced);
  await page.evaluate(() => { window.overlayTest.fail = "answer_approval"; });
  await page.locator(".approve").focus();
  await page.keyboard.press("Space");
  await expect(page.locator(".approval-error")).toContainText("Fixture command failed");
  await expect(page.locator(".approve")).toBeEnabled();
  await expect(page.locator(".deny")).toBeEnabled();
});

test("removing the focused session selects a surviving card or Refresh when empty", async ({ page }) => {
  await page.locator(".deny").focus();
  await replay(page, snapshots.removed);
  await expect(card(page, "AO-02")).toBeFocused();
  await replay(page, snapshots.empty);
  await expect(page.getByRole("button", { name: "Refresh sessions" })).toBeFocused();
  await expect(page.locator("#btn-kill-all")).toBeDisabled();
  expect(await calls(page, "answer_approval")).toEqual([]);
});

test("keyboard Close honors cancellation, targets the session, and recovers from failure", async ({ page }) => {
  const close = card(page, "AO-01").getByRole("button");
  await close.focus();
  page.once("dialog", dialog => dialog.dismiss());
  await page.keyboard.press("Enter");
  expect(await calls(page, "kill_session")).toEqual([]);
  await page.evaluate(() => { window.overlayTest.fail = "kill_session"; });
  page.on("dialog", dialog => dialog.accept());
  await page.keyboard.press("Space");
  await expect(close).toBeEnabled();
  await expect(card(page, "AO-01")).not.toHaveClass(/exiting/);
  await page.evaluate(() => { window.overlayTest.fail = null; });
  await close.focus();
  await page.keyboard.press("Enter");
  await expect(card(page, "AO-01")).toHaveCount(0);
  await expect(card(page, "AO-02")).toBeFocused();
  expect((await calls(page, "kill_session")).map(call => call.args)).toEqual([{ sessionId: "AO-01" }, { sessionId: "AO-01" }]);
});

test("Refresh retains focus and collapsing returns it to the keyboard toggle", async ({ page }) => {
  await page.locator("#btn-refresh").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator("#btn-refresh")).toBeFocused();
  expect(await calls(page, "get_sessions")).toHaveLength(2);
  await page.keyboard.press("Tab");
  await page.keyboard.press("Space");
  await expect(page.locator("#pill")).toBeFocused();
  await expect(page.locator("#pill")).toHaveAttribute("aria-expanded", "false");
  await page.keyboard.press("Space");
  await expect(page.locator("#panel")).toBeVisible();
});
