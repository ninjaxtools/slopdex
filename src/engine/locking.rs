use super::*;

/// Writable engines also read under a shared lock. Only workspace publication
/// needs exclusivity. Global artifacts have their own per-key coordination.
pub(super) struct IndexLock(Arc<fs::File>);

/// How long a failed upgrade may take to get its shared lock back. Restoring
/// can only contend with a writer that won the unlock window; its hold is a
/// bounded publication, and the engine revalidates its workspace stamp after.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

impl IndexLock {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        lock_index(&file, true)?;
        Ok(Self(Arc::new(file)))
    }

    /// Re-acquire the shared lock released for an upgrade attempt.
    fn restore_shared(&self) -> Result<()> {
        let restored = registry::lock_shared_until(&self.0, Instant::now() + RESTORE_TIMEOUT)
            .context("Restore shared index lock")?;
        ensure!(restored, "Timed out restoring the shared index lock");
        Ok(())
    }

    pub(super) fn write(&self) -> Result<WriteLock> {
        // Explicitly release before upgrading, including on Windows. Retaining
        // a shared lock while waiting for exclusivity deadlocks other upgraders.
        FileExt::unlock(&*self.0)?;
        match lock_index(&self.0, false) {
            Ok(()) => Ok(WriteLock(Arc::clone(&self.0))),
            Err(error) => {
                // The guard exists only once exclusivity is held, so this
                // failure path restores the shared lock explicitly (and
                // fallibly) instead of through a panicking Drop.
                self.restore_shared()?;
                Err(error)
            }
        }
    }

    pub(super) fn try_write(&self) -> Result<Option<WriteLock>> {
        self.write_for(Duration::ZERO)
    }

    pub(super) fn write_for(&self, timeout: Duration) -> Result<Option<WriteLock>> {
        FileExt::unlock(&*self.0)?;
        if registry::lock_exclusive_until(&self.0, Instant::now() + timeout)? {
            Ok(Some(WriteLock(Arc::clone(&self.0))))
        } else {
            // Not acquired: put the shared lock back before reporting None.
            // This is instant against other readers; against a writer that
            // grabbed exclusivity in the unlock window it waits, bounded, for
            // that writer's hold -- never forever, and never via Drop.
            self.restore_shared()?;
            Ok(None)
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // A concurrently forked child may still have the file open before exec.
        let _ = FileExt::unlock(&*self.0);
    }
}

pub(super) struct WriteLock(Arc<fs::File>);

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Lock conversion is not atomic on every platform. The engine
        // validates its workspace stamp again after the shared lock has been
        // restored. Restore is bounded and best-effort: panicking here could
        // abort the process if the guard drops during unwinding.
        let _ = FileExt::unlock(&*self.0);
        match registry::lock_shared_until(&self.0, Instant::now() + RESTORE_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => ui::warning("slopdex: warning: timed out restoring the shared index lock"),
            Err(error) => ui::warning(format_args!(
                "slopdex: warning: could not restore the shared index lock: {error}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exclusive_available(path: &Path) -> bool {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                true
            }
            Err(_) => false,
        }
    }

    #[test]
    fn failed_upgrade_restores_the_shared_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.lock");
        let first = IndexLock::open(&path).unwrap();
        let second = IndexLock::open(&path).unwrap();
        // Another shared holder blocks the upgrade; giving up must leave this
        // engine holding its shared lock again.
        assert!(first.try_write().unwrap().is_none());
        drop(second);
        assert!(
            !exclusive_available(&path),
            "shared lock was not restored after a failed upgrade"
        );
        // A successful upgrade's guard restores shared locking on drop too.
        let guard = first.write().unwrap();
        assert!(!exclusive_available(&path));
        drop(guard);
        assert!(
            !exclusive_available(&path),
            "shared lock was not restored after the write guard dropped"
        );
        drop(first);
        assert!(exclusive_available(&path));
    }
}
