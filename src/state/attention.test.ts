// @vitest-environment jsdom
// attention.ts touches localStorage (one-time legacy-flag migration) at import
// time; the default "node" environment for this directory has no such global.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn().mockResolvedValue(undefined) }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn().mockResolvedValue(true),
  requestPermission: vi.fn().mockResolvedValue("granted"),
  sendNotification: vi.fn(),
}));

const capturedListeners: Array<(event: unknown) => void> = [];
vi.mock("./events", () => ({
  onBusEvent: (cb: (event: unknown) => void) => {
    capturedListeners.push(cb);
    return () => {};
  },
}));

function dispatch(event: unknown) {
  for (const listener of capturedListeners) listener(event);
}

const { useAttention } = await import("./attention");
const { sendNotification } = await import("@tauri-apps/plugin-notification");
const { invoke } = await import("@tauri-apps/api/core");
import type { AttentionItem } from "../types/attention";

const item = (id: string, kind: AttentionItem["kind"]): AttentionItem => ({
  id,
  kind,
  target: kind === "gate" ? "gate" : "chat",
  branch: "impl/a",
  session_id: null,
  message: `${kind} item`,
  created_at: "2026-08-14T00:00:00Z",
});

const sessionFailed = (branch: string) => ({
  type: "session.status_changed",
  data: { session_id: "s1", branch, status: "failed" },
});

describe("notification digest", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.mocked(sendNotification).mockClear();
    useAttention.setState({ notificationsEnabled: true, digestEnabled: false });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("sends one notification per item when digest mode is off", () => {
    dispatch(sessionFailed("impl/a"));
    dispatch(sessionFailed("impl/b"));
    expect(sendNotification).toHaveBeenCalledTimes(2);
  });

  it("does not notify at all when notifications are disabled, digest or not", () => {
    useAttention.setState({ notificationsEnabled: false, digestEnabled: true });
    dispatch(sessionFailed("impl/a"));
    vi.advanceTimersByTime(10_000);
    expect(sendNotification).not.toHaveBeenCalled();
  });

  it("coalesces a burst into a single 'N items' notification", () => {
    useAttention.setState({ digestEnabled: true });
    dispatch(sessionFailed("impl/a"));
    dispatch(sessionFailed("impl/b"));
    dispatch(sessionFailed("impl/c"));
    expect(sendNotification).not.toHaveBeenCalled();

    vi.advanceTimersByTime(4000);
    expect(sendNotification).toHaveBeenCalledTimes(1);
    expect(sendNotification).toHaveBeenCalledWith({
      title: "MaestroIDE",
      body: "3 items need your attention",
    });
  });

  it("keeps the specific message when only one item lands in the window", () => {
    useAttention.setState({ digestEnabled: true });
    dispatch(sessionFailed("impl/a"));
    vi.advanceTimersByTime(4000);
    expect(sendNotification).toHaveBeenCalledTimes(1);
    expect(sendNotification).toHaveBeenCalledWith({
      title: "MaestroIDE",
      body: "Session failed on impl/a",
    });
  });

  it("restarts the window on each new item instead of flushing on a fixed schedule", () => {
    useAttention.setState({ digestEnabled: true });
    dispatch(sessionFailed("impl/a"));
    vi.advanceTimersByTime(3000);
    dispatch(sessionFailed("impl/b"));
    vi.advanceTimersByTime(3000);
    expect(sendNotification).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1000);
    expect(sendNotification).toHaveBeenCalledTimes(1);
    expect(sendNotification).toHaveBeenCalledWith({
      title: "MaestroIDE",
      body: "2 items need your attention",
    });
  });
});

describe("dismissAll", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockClear();
    vi.mocked(invoke).mockResolvedValue(undefined);
  });

  it("clears everything except gates — they block a command until answered", async () => {
    useAttention.setState({
      items: [item("g1", "gate"), item("f1", "session_failed"), item("r1", "red_team_ready")],
    });
    await useAttention.getState().dismissAll();
    expect(invoke).toHaveBeenCalledWith("dismiss_all_attention");
    const left = useAttention.getState().items;
    expect(left.map((i) => i.id)).toEqual(["g1"]);
  });

  it("keeps the list intact and surfaces the error when the backend fails", async () => {
    useAttention.setState({
      items: [item("f1", "session_failed")],
      error: null,
    });
    vi.mocked(invoke).mockRejectedValueOnce(new Error("core down"));
    await useAttention.getState().dismissAll();
    expect(useAttention.getState().items).toHaveLength(1);
    expect(useAttention.getState().error).toContain("core down");
  });
});
