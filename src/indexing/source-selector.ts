import type { IndexDatabase } from "../storage/database.js";
import { CodeIndexError, GitDivergenceError } from "../errors.js";
import { parseCallables } from "../parser/callable-parser.js";
import type { GitRepository } from "../repository.js";
import type { CrossSearchSourceFilter, IndexedFunction } from "../types.js";
import { compileNameRegex, groupBy, normalizeRelativePath } from "../utils.js";

export class SourceSelector {
  readonly #database: IndexDatabase;
  readonly #git: GitRepository;
  readonly #rootDir: string;
  readonly #onWarning: (message: string) => void;

  public constructor(
    database: IndexDatabase,
    git: GitRepository,
    rootDir: string,
    onWarning: (message: string) => void,
  ) {
    this.#database = database;
    this.#git = git;
    this.#rootDir = rootDir;
    this.#onWarning = onWarning;
  }

  public async sourceFunctions(filter: CrossSearchSourceFilter): Promise<IndexedFunction[]> {
    const nameRegex = compileNameRegex(filter.nameRegex);
    const sourcePath = filter.path ? normalizeRelativePath(this.#rootDir, filter.path) : undefined;
    const uncommitted = filter.type === "uncommitted" || (filter.type === "changed-since" && filter.uncommitted);
    const functions = filter.type === "changed-since"
      ? await this.#functionsChangedSince(filter.commit)
      : this.#database.allFunctions();
    return functions.filter((callable) =>
      (!uncommitted || callable.sourceMode === "working-tree")
      && (!sourcePath || callable.path === sourcePath || callable.path.startsWith(`${sourcePath}/`))
      && (!nameRegex || nameRegex.test(callable.qualifiedName)));
  }

  async #functionsChangedSince(baseRef: string): Promise<IndexedFunction[]> {
    await this.#git.assertRepository();
    const checkpoint = this.#database.getCheckpoint();
    if (!checkpoint) throw new CodeIndexError("changed-since requires an index with a Git checkpoint.");
    const base = await this.#git.resolveCommit(baseRef);
    if (!(await this.#git.isAncestor(base, checkpoint))) {
      throw new GitDivergenceError(base, checkpoint);
    }

    const changes = await this.#git.diff(base, checkpoint);
    const baseTree = await this.#git.listTree(base);
    const comparisonPaths = new Map<string, { basePath: string | null; pathChanged: boolean }>();
    for (const change of changes) {
      if (change.status === "D") continue;
      if (change.status === "R") comparisonPaths.set(change.path, { basePath: change.oldPath, pathChanged: true });
      else if (change.status === "A" || change.status === "C") comparisonPaths.set(change.path, { basePath: null, pathChanged: false });
      else comparisonPaths.set(change.path, { basePath: change.path, pathChanged: false });
    }
    for (const dirtyFile of this.#database.getWorkingTreeFiles()) {
      const basePath = dirtyFile.previousPath ?? comparisonPaths.get(dirtyFile.path)?.basePath ?? dirtyFile.path;
      comparisonPaths.set(dirtyFile.path, { basePath, pathChanged: basePath !== dirtyFile.path });
    }

    const current = this.#database.functionsForPaths([...comparisonPaths.keys()]);
    const baseFunctionsByCurrentPath = new Map<string, ReturnType<typeof parseCallables>>();
    for (const [currentPath, { basePath }] of comparisonPaths) {
      if (!basePath) {
        baseFunctionsByCurrentPath.set(currentPath, []);
        continue;
      }
      const entry = baseTree.get(basePath);
      if (!entry) {
        baseFunctionsByCurrentPath.set(currentPath, []);
        continue;
      }
      const content = (await this.#git.readBlob(entry.oid)).toString("utf8");
      baseFunctionsByCurrentPath.set(currentPath, parseCallables(basePath, content, this.#onWarning));
    }
    const currentByPath = groupBy(current, (callable) => callable.path);
    const changedIds = new Set<number>();
    for (const [currentPath, currentFunctions] of currentByPath) {
      const comparison = comparisonPaths.get(currentPath)!;
      for (const id of changedFunctionIds(
        currentFunctions,
        baseFunctionsByCurrentPath.get(currentPath) ?? [],
        comparison.pathChanged,
      )) {
        changedIds.add(id);
      }
    }
    return current.filter((callable) => changedIds.has(callable.id));
  }
}

function changedFunctionIds(
  current: readonly IndexedFunction[],
  base: readonly Pick<IndexedFunction, "qualifiedName" | "kind" | "sourceHash">[],
  pathChanged: boolean,
): number[] {
  if (pathChanged) return current.map((callable) => callable.id);
  const currentGroups = groupBy(current, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const baseGroups = groupBy(base, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const changed: number[] = [];
  for (const [key, currentGroup] of currentGroups) {
    const baseGroup = baseGroups.get(key) ?? [];
    const unmatchedBase = new Set(baseGroup.map((_, index) => index));
    const unmatchedCurrent = new Set(currentGroup.map((_, index) => index));
    for (let currentIndex = 0; currentIndex < currentGroup.length; currentIndex += 1) {
      const exactBaseIndex = [...unmatchedBase].find(
        (baseIndex) => baseGroup[baseIndex]!.sourceHash === currentGroup[currentIndex]!.sourceHash,
      );
      if (exactBaseIndex !== undefined) {
        unmatchedBase.delete(exactBaseIndex);
        unmatchedCurrent.delete(currentIndex);
      }
    }
    for (const currentIndex of unmatchedCurrent) changed.push(currentGroup[currentIndex]!.id);
  }
  return changed;
}
