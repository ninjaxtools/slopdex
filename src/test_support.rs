//! Filesystem capabilities needed by Unix-only integration fixtures.
use std::{ffi::OsStr, fs, io, os::unix::ffi::OsStrExt, path::Path};

/// Unix paths can hold arbitrary bytes in memory, but not every Unix filesystem
/// accepts them on disk (notably macOS APFS/HFS+). Probe only within the fixture.
pub(crate) fn supports_non_utf8_names(root: &Path) -> io::Result<bool> {
    let probe = root.join(OsStr::from_bytes(b".slopdex-non-utf8-\xff"));
    match fs::write(&probe, b"") {
        Ok(()) => {
            fs::remove_file(probe)?;
            Ok(true)
        }
        // Darwin's EILSEQ. Do not hide permission, missing-directory, or other
        // setup failures, and do not assume every macOS volume rejects names.
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            eprintln!("skipping non-UTF-8 on-disk fixture: filesystem rejects undecodable names");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[test]
fn non_utf8_probe_cleans_up_and_preserves_setup_errors() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let _supported = supports_non_utf8_names(temp.path())?;
    assert_eq!(fs::read_dir(temp.path())?.count(), 0);
    let error = supports_non_utf8_names(&temp.path().join("missing")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    Ok(())
}
