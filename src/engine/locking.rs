use super::*;

/// Writable engines also read under a shared lock. Only workspace publication
/// needs exclusivity. Global artifacts have their own per-key coordination.
pub(super) struct IndexLock(Arc<fs::File>);

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

    pub(super) fn write(&self) -> Result<WriteLock> {
        // Explicitly release before upgrading, including on Windows. Retaining
        // a shared lock while waiting for exclusivity deadlocks other upgraders.
        FileExt::unlock(&*self.0)?;
        let guard = WriteLock(Arc::clone(&self.0));
        lock_index(&self.0, false)?;
        Ok(guard)
    }

    pub(super) fn try_write(&self) -> Result<Option<WriteLock>> {
        self.write_for(Duration::ZERO)
    }

    pub(super) fn write_for(&self, timeout: Duration) -> Result<Option<WriteLock>> {
        FileExt::unlock(&*self.0)?;
        let guard = WriteLock(Arc::clone(&self.0));
        if registry::lock_exclusive_until(&self.0, Instant::now() + timeout)? {
            Ok(Some(guard))
        } else {
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
        // Lock conversion is not atomic on every platform. The engine validates
        // its workspace stamp again after the shared lock has been restored.
        let _ = FileExt::unlock(&*self.0);
        while let Err(error) = FileExt::lock_shared(&*self.0) {
            if error.kind() != std::io::ErrorKind::Interrupted {
                panic!("Restore shared index lock: {error}");
            }
        }
    }
}
