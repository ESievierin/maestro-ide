import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./events", () => ({ onBusEvent: vi.fn(() => () => {}) }));

const { useBranchUsage } = await import("./branchUsage");
const { invoke } = await import("@tauri-apps/api/core");

const row = (branch: string, cost: number) => ({
  branch,
  totals: { cost_usd: cost, turns: 1, input_tokens: 10, output_tokens: 10 },
});

describe("branchUsage", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
    useBranchUsage.setState({ costByBranch: {} });
  });

  it("maps the store's per-branch aggregate into costByBranch", async () => {
    vi.mocked(invoke).mockResolvedValue([row("impl/a", 1.25), row("redteam/impl-a", 0.4)]);
    await useBranchUsage.getState().fetch();
    expect(invoke).toHaveBeenCalledWith("get_usage_by_branch");
    expect(useBranchUsage.getState().costByBranch).toEqual({
      "impl/a": 1.25,
      "redteam/impl-a": 0.4,
    });
  });

  it("keeps the last known values when the backend call fails", async () => {
    useBranchUsage.setState({ costByBranch: { "impl/a": 2.0 } });
    vi.mocked(invoke).mockRejectedValue(new Error("core down"));
    await useBranchUsage.getState().fetch();
    expect(useBranchUsage.getState().costByBranch).toEqual({ "impl/a": 2.0 });
  });
});
