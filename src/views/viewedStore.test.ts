// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";
import { loadViewed, saveViewed } from "./viewedStore";

describe("viewedStore", () => {
  beforeEach(() => localStorage.clear());

  it("round-trips a viewed set for the same merge-base", () => {
    saveViewed("impl/T-1", "abc123", new Set(["a.rs", "b.ts"]));
    expect([...loadViewed("impl/T-1", "abc123")].sort()).toEqual(["a.rs", "b.ts"]);
  });

  it("a moved merge-base resets the state", () => {
    saveViewed("impl/T-1", "abc123", new Set(["a.rs"]));
    expect(loadViewed("impl/T-1", "def456").size).toBe(0);
  });

  it("branches don't leak into each other", () => {
    saveViewed("impl/T-1", "abc123", new Set(["a.rs"]));
    expect(loadViewed("impl/T-2", "abc123").size).toBe(0);
  });

  it("an emptied set removes the entry", () => {
    saveViewed("impl/T-1", "abc123", new Set(["a.rs"]));
    saveViewed("impl/T-1", "abc123", new Set());
    expect(localStorage.getItem("maestro.viewed.impl/T-1")).toBeNull();
  });

  it("garbage in storage loads as empty", () => {
    localStorage.setItem("maestro.viewed.impl/T-1", "{not json");
    expect(loadViewed("impl/T-1", "abc123").size).toBe(0);
    localStorage.setItem("maestro.viewed.impl/T-1", JSON.stringify({ files: "nope" }));
    expect(loadViewed("impl/T-1", "abc123").size).toBe(0);
  });
});
