//! Connect the REPL to a running `ptywright serve --socket <path>`.
//!
//! Mirrors the server-side cfg split already used by `src/main.rs::serve_socket`:
//! Unix domain sockets on macOS / Linux, named pipes via the `interprocess`
//! crate on Windows. Returns boxed `Read` + `Write` halves wired into the
//! same connection so the [`crate::repl::transport::RpcClient`] can run its
//! reader thread.

use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// Owned reader / writer pair backed by a single duplex connection.
pub struct SocketTransport {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

#[cfg(unix)]
pub fn connect(path: &Path) -> Result<SocketTransport> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::Rpc(format!(
            "no ptywright server is listening at {path}.\n\
             \n\
             Start one in another terminal:\n  \
             ptywright serve --socket {path} &\n\
             \n\
             …or pipe a child server through stdio in one command:\n  \
             ptywright repl --stdio -- ptywright serve --stdio",
            path = path.display(),
        )),
        std::io::ErrorKind::ConnectionRefused => Error::Rpc(format!(
            "socket {path} exists but is not accepting connections — \
             a previous server probably exited without cleaning up. Run \
             `ptywright serve` (or `ptywright serve --socket {path}`) and \
             it will reclaim the stale socket automatically before binding.",
            path = path.display(),
        )),
        std::io::ErrorKind::PermissionDenied => Error::Rpc(format!(
            "permission denied connecting to {path}. Was the server started \
             by a different user?",
            path = path.display(),
        )),
        _ => Error::Rpc(format!(
            "connect to ptywright serve socket {}: {error}",
            path.display()
        )),
    })?;
    // Use `try_clone` so the reader thread can own one handle while the
    // writer-side `Mutex<Box<dyn Write + Send>>` in `RpcClient` owns the
    // other. Closing either half terminates the connection.
    let reader = stream
        .try_clone()
        .map_err(|error| Error::Rpc(format!("clone socket handle: {error}")))?;
    Ok(SocketTransport {
        reader: Box::new(reader),
        writer: Box::new(stream),
    })
}

#[cfg(windows)]
pub fn connect(path: &Path) -> Result<SocketTransport> {
    use interprocess::local_socket::{GenericFilePath, Stream as LocalStream, prelude::*};

    let name = path
        .as_os_str()
        .to_fs_name::<GenericFilePath>()
        .map_err(|error| {
            Error::Rpc(format!(
                "convert socket path `{}` for named-pipe connect: {error}",
                path.display()
            ))
        })?;
    let stream = LocalStream::connect(name).map_err(|error| {
        Error::Rpc(format!(
            "connect to ptywright named pipe {}: {error}",
            path.display()
        ))
    })?;
    let (reader, writer) = stream.split();
    Ok(SocketTransport {
        reader: Box::new(reader),
        writer: Box::new(writer),
    })
}

#[cfg(not(any(unix, windows)))]
pub fn connect(path: &Path) -> Result<SocketTransport> {
    Err(Error::Rpc(format!(
        "--socket is not supported on this platform yet (requested {})",
        path.display()
    )))
}
