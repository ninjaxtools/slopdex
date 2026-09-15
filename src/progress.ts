import type { IndexProgress, IndexProgressPhase } from "./types.js";

const CLEAR_LINE = "\r\x1b[K";
const BAR_WIDTH = 28;

const PHASE_LABELS: Record<IndexProgressPhase, string> = {
  vectors: "vectors",
  descriptions: "descriptions",
  "description-vectors": "description vectors",
  "similarity-cache": "similarity cache",
};

let active: TerminalProgress | undefined;

export function renderProgress(progress: IndexProgress): string {
  const ratio = progress.total > 0 ? Math.min(1, progress.completed / progress.total) : 0;
  const filled = Math.round(ratio * BAR_WIDTH);
  const percent = Math.floor(ratio * 100);
  const bar = `${"=".repeat(filled)}${" ".repeat(BAR_WIDTH - filled)}`;
  return `${PHASE_LABELS[progress.phase]} [${bar}] ${String(percent).padStart(3)}% ${progress.completed}/${progress.total}`;
}

export function writeStderr(message: string): void {
  const bar = active;
  bar?.erase();
  process.stderr.write(message);
  bar?.draw();
}

export function clearProgress(): void {
  if (!active) return;
  active.erase();
  active.abandon();
  active = undefined;
}

export class TerminalProgress {
  #phase: IndexProgressPhase | undefined;
  #completed = 0;
  #total = 0;

  public update(progress: IndexProgress): void {
    if (process.stderr.isTTY !== true || progress.total <= 0) return;
    if (progress.completed === 0 || progress.phase !== this.#phase) {
      this.#finish();
      this.#phase = progress.phase;
      this.#completed = Math.min(progress.completed, progress.total);
      this.#total = progress.total;
      active = this;
      this.draw();
      if (this.#completed >= this.#total) this.#finish();
      return;
    }
    this.#completed = Math.max(this.#completed, progress.completed);
    this.#total = progress.total;
    this.draw();
    if (progress.completed >= progress.total) this.#finish();
  }

  public draw(): void {
    if (this.#phase === undefined || process.stderr.isTTY !== true) return;
    process.stderr.write(`${CLEAR_LINE}${renderProgress({
      phase: this.#phase,
      completed: this.#completed,
      total: this.#total,
    })}`);
  }

  public erase(): void {
    if (process.stderr.isTTY === true) process.stderr.write(CLEAR_LINE);
  }

  public abandon(): void {
    this.#phase = undefined;
  }

  #finish(): void {
    if (this.#phase === undefined) return;
    if (process.stderr.isTTY === true) process.stderr.write("\n");
    this.#phase = undefined;
    if (active === this) active = undefined;
  }
}
