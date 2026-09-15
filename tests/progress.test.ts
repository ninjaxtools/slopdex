import { afterEach, describe, expect, it, vi } from "vitest";

import { clearProgress, renderProgress, TerminalProgress, writeStderr } from "../src/progress.js";

function stubTty(value: boolean): () => void {
  const descriptor = Object.getOwnPropertyDescriptor(process.stderr, "isTTY");
  Object.defineProperty(process.stderr, "isTTY", { configurable: true, value });
  return () => {
    if (descriptor) Object.defineProperty(process.stderr, "isTTY", descriptor);
    else delete (process.stderr as { isTTY?: boolean }).isTTY;
  };
}

describe("terminal progress", () => {
  afterEach(() => {
    clearProgress();
    vi.restoreAllMocks();
  });

  it("renders a labeled percentage bar for each phase", () => {
    expect(renderProgress({ phase: "vectors", completed: 0, total: 4 })).toContain("vectors [");
    expect(renderProgress({ phase: "vectors", completed: 2, total: 4 })).toContain(" 50% 2/4");
    expect(renderProgress({ phase: "descriptions", completed: 4, total: 4 })).toContain("100% 4/4");
    expect(renderProgress({ phase: "description-vectors", completed: 1, total: 4 })).toContain("description vectors [");
    expect(renderProgress({ phase: "similarity-cache", completed: 1, total: 4 })).toContain("similarity cache [");
  });

  it("does not write progress when stderr is not interactive", () => {
    const write = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    const restore = stubTty(false);
    try {
      const progress = new TerminalProgress();
      progress.update({ phase: "vectors", completed: 0, total: 2 });
      progress.update({ phase: "vectors", completed: 2, total: 2 });
      expect(write).not.toHaveBeenCalled();
    } finally {
      restore();
    }
  });

  it("animates a bar to completion and clears the line around notices", () => {
    const write = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    const restore = stubTty(true);
    let writes: string[] = [];
    try {
      const progress = new TerminalProgress();
      progress.update({ phase: "vectors", completed: 0, total: 4 });
      progress.update({ phase: "vectors", completed: 2, total: 4 });
      const before = write.mock.calls.length;
      writeStderr("slopdex: notice: example\n");
      expect(write.mock.calls.slice(before).map(([value]) => String(value))).toEqual([
        "\r\x1b[K",
        "slopdex: notice: example\n",
        "\r\x1b[Kvectors [==============              ]  50% 2/4",
      ]);
      progress.update({ phase: "vectors", completed: 4, total: 4 });
      writes = write.mock.calls.map(([value]) => String(value));
    } finally {
      restore();
    }
    const output = writes.join("");
    expect(output).toContain(" 50% 2/4");
    expect(output).toContain("100% 4/4");
    expect(writes.at(-1)).toBe("\n");
  });

  it("clears an active bar when progress is abandoned", () => {
    const write = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    const restore = stubTty(true);
    try {
      const progress = new TerminalProgress();
      progress.update({ phase: "descriptions", completed: 1, total: 4 });
      write.mockClear();
      clearProgress();
      expect(write.mock.calls.map(([value]) => String(value))).toEqual(["\r\x1b[K"]);
    } finally {
      restore();
    }
  });
});
