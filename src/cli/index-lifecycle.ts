import { existsSync, rmSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import { isDescriptionProviderName, resolveConfig } from "../config.js";
import { CodeIndexError, GitUnavailableError, IncompatibleIndexError } from "../errors.js";
import type { CodeIndex } from "../code-index.js";
import type { CodeIndexOptions, DescriptionProfile, UpdateStats } from "../types.js";
import { createDescriptionProvider, type DescriptionRefreshHooks } from "./provider-factories.js";

export async function ensureIndexUpdated(
  options: CodeIndexOptions,
  label: string,
  target: string,
  rebuildOnDivergence: boolean,
  forceRebuild: boolean,
  noReindex: boolean,
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats> {
  const initialized = await initializeMissingIndex(options, label, target, noReindex, hooks);
  if (initialized) return initialized;
  const { CodeIndex } = await import("../code-index.js");
  try {
    const index = new CodeIndex(options);
    try {
      await hooks?.beforeRefresh?.(index);
      const stats = await refreshIndex(index, label, target, rebuildOnDivergence, noReindex);
      await hooks?.afterRefresh?.(index);
      return stats;
    } finally {
      index.close();
    }
  } catch (error) {
    if (!forceRebuild || !(error instanceof IncompatibleIndexError)) throw error;
    process.stderr.write(
      `slopdex: warning: ${label} is incompatible (${error.message}); rebuilding automatically because --force-reindex was specified.\n`,
    );
    const indexPath = resolveIndexPath(options);
    const descriptionProfile = descriptionProfileForRebuild(indexPath, options.rootDir);
    try {
      const { resetIndexState } = await import("../storage/database.js");
      resetIndexState(indexPath, options.rootDir, options.provider.profile);
    } catch (resetError) {
      if (!(resetError instanceof IncompatibleIndexError)) throw resetError;
      removeIndexArtifacts(indexPath);
    }
    return initializeIndex(options, indexPath, label, target, noReindex, descriptionProfile, hooks);
  }
}

async function initializeMissingIndex(
  options: CodeIndexOptions,
  label: string,
  target: string,
  noReindex: boolean,
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats | null> {
  const indexPath = resolveIndexPath(options);
  if (existsSync(indexPath)) return null;

  process.stderr.write(
    `slopdex: ${label} not found at ${indexPath}; initializing automatically from ${target}${noReindex ? "" : " and the working tree"}.\n`,
  );
  return initializeIndex(options, indexPath, label, target, noReindex, null, hooks);
}

async function initializeIndex(
  options: CodeIndexOptions,
  indexPath: string,
  label: string,
  target: string,
  noReindex: boolean,
  descriptionProfile: DescriptionProfile | null = null,
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats> {
  const { CodeIndex } = await import("../code-index.js");
  const rebuiltDescriptionProvider = descriptionProfile && !options.descriptionProvider
    ? await createDescriptionProvider(resolveConfig(options.rootDir, {
      indexPath,
      descriptionProvider: descriptionProfile.provider,
      descriptionModel: descriptionProfile.model,
      parallelism: options.parallelism,
      ...(options.verbose ? { verbose: true } : {}),
    }))
    : undefined;
  const index = new CodeIndex({
    ...options, indexPath,
    ...(rebuiltDescriptionProvider ? { descriptionProvider: rebuiltDescriptionProvider } : {}),
  });
  try {
    if (descriptionProfile) await index.useDescriptions();
    await hooks?.beforeRefresh?.(index);
    const stats = await refreshIndex(index, label, target, false, noReindex);
    await hooks?.afterRefresh?.(index);
    return stats;
  } finally {
    index.close();
  }
}

function descriptionProfileForRebuild(indexPath: string, rootDir: string): DescriptionProfile | null {
  const db = new DatabaseSync(indexPath, { readOnly: true });
  try {
    const metadata = new Map((db.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("root_dir") !== path.resolve(rootDir) || metadata.get("descriptions_enabled") !== "true") return null;
    const profile = JSON.parse(metadata.get("description_profile")!) as DescriptionProfile;
    if (!isDescriptionProviderName(profile.provider)) {
      throw new CodeIndexError("Rebuilding this description index requires its custom description provider through the library API.");
    }
    return profile;
  } finally {
    db.close();
  }
}

export function storedDescriptionProfile(indexPath: string, rootDir: string): DescriptionProfile | null {
  if (!existsSync(indexPath)) return null;
  const db = new DatabaseSync(indexPath, { readOnly: true });
  try {
    const metadata = new Map((db.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("root_dir") !== path.resolve(rootDir)) return null;
    const value = metadata.get("description_profile");
    if (!value) return null;
    const profile = JSON.parse(value) as Partial<DescriptionProfile>;
    if (typeof profile.model !== "string" || !profile.model || !isDescriptionProviderName(profile.provider ?? "")) return null;
    return {
      provider: profile.provider!,
      model: profile.model,
      strategyVersion: typeof profile.strategyVersion === "string" ? profile.strategyVersion : "callable-purpose-v2",
    };
  } catch {
    return null;
  } finally {
    db.close();
  }
}

async function refreshIndex(
  index: CodeIndex,
  label: string,
  target: string,
  rebuildOnDivergence: boolean,
  noReindex: boolean,
): Promise<UpdateStats> {
  try {
    return await index.updateFromGit({ target, rebuildOnDivergence, includeWorkingTree: !noReindex });
  } catch (error) {
    if (!(error instanceof GitUnavailableError)) throw error;
    const status = index.status();
    if (noReindex && status.fileCount > 0) {
      process.stderr.write(
        `slopdex: warning: no Git repository is available for ${label}; full working-tree re-index skipped because --no-reindex was specified.\n`,
      );
      return {
        filesUpdated: 0,
        filesDeleted: 0,
        functionsAdded: 0,
        functionsUpdated: 0,
        functionsDeleted: 0,
        embeddingsCreated: 0,
        checkpoint: status.gitCheckpoint,
      };
    }
    process.stderr.write(
      `slopdex: warning: no Git repository is available for ${label}; re-indexing all source files from the working tree.\n`,
    );
    return await index.updateFromWorkingTree();
  }
}

export function resolveIndexPath(options: CodeIndexOptions): string {
  return path.resolve(options.indexPath ?? path.join(path.resolve(options.rootDir), ".slopdex", "index.sqlite"));
}

function removeIndexArtifacts(indexPath: string): void {
  for (const suffix of ["", "-shm", "-wal", "-journal"]) rmSync(`${indexPath}${suffix}`, { force: true });
}
