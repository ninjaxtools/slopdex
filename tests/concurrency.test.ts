import { describe, expect, it } from "vitest";

import { forEachConcurrent } from "../src/utils.js";

describe("forEachConcurrent", () => {
  it("runs work with the configured concurrency limit", async () => {
    let active = 0;
    let maximum = 0;
    const completed: number[] = [];
    await forEachConcurrent([0, 1, 2, 3, 4, 5], 2, async (value) => {
      active += 1;
      maximum = Math.max(maximum, active);
      await new Promise((resolve) => setTimeout(resolve, 2));
      completed.push(value);
      active -= 1;
    });
    expect(maximum).toBe(2);
    expect([...completed].sort((left, right) => left - right)).toEqual([0, 1, 2, 3, 4, 5]);
  });

  it("stops scheduling work after a failure and still completes in-flight work", async () => {
    const started: number[] = [];
    const completed: number[] = [];
    const result = forEachConcurrent([0, 1, 2, 3, 4, 5], 2, async (value) => {
      started.push(value);
      if (value === 1) throw new Error("intentional failure");
      await new Promise((resolve) => setTimeout(resolve, 2));
      completed.push(value);
    });
    await expect(result).rejects.toThrow("intentional failure");
    expect(completed).toEqual([0]);
    expect(started.length).toBeLessThan(6);
  });

  it("propagates caller aborts into worker signals", async () => {
    const controller = new AbortController();
    await expect(forEachConcurrent([0, 1, 2, 3], 2, async (_value, _index, signal) => {
      controller.abort(new Error("stopped"));
      signal.throwIfAborted();
    }, controller.signal)).rejects.toThrow("stopped");
  });

  it("rejects invalid concurrency limits", async () => {
    await expect(forEachConcurrent([0], 0, async () => {})).rejects.toThrow(/parallelism must be a positive integer/);
  });
});
