use super::*;
use serde_json::json;

#[test]
fn stdout_boundary_only_ignores_its_own_broken_pipes() {
    struct FailingWriter {
        write_error: Option<io::ErrorKind>,
        flush_error: Option<io::ErrorKind>,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            match self.write_error {
                Some(kind) => Err(kind.into()),
                None => Ok(bytes.len()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.flush_error {
                Some(kind) => Err(kind.into()),
                None => Ok(()),
            }
        }
    }

    for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::PermissionDenied] {
        for json in [false, true] {
            for on_flush in [false, true] {
                let writer = FailingWriter {
                    write_error: (!on_flush).then_some(kind),
                    flush_error: on_flush.then_some(kind),
                };
                let result = with_stdout(writer, |out| {
                    if json {
                        print_json(out, &json!({"output": "value"}))
                    } else {
                        writeln!(out, "output").map_err(Into::into)
                    }
                });
                assert_eq!(
                    result.is_ok(),
                    kind == io::ErrorKind::BrokenPipe,
                    "kind={kind:?}, json={json}, on_flush={on_flush}: {result:?}"
                );
            }
        }
    }

    // Provider/network failures have no stdout marker, even when they have
    // the same I/O kind or are wrapped by JSON serialization and context.
    for error in [
        anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe)),
        anyhow::Error::from(serde_json::Error::io(io::ErrorKind::BrokenPipe.into())),
    ] {
        let result = with_stdout(Vec::new(), |_| Err(error.context("provider failed")));
        assert!(result.is_err());
    }

    let result = with_stdout(
        FailingWriter {
            write_error: Some(io::ErrorKind::BrokenPipe),
            flush_error: None,
        },
        |out| {
            let _ = writeln!(out, "output");
            Err(io::Error::from(io::ErrorKind::BrokenPipe).into())
        },
    );
    assert!(result.is_err(), "an unrelated error must still propagate");
}
