//! Same-user local IPC transport for the bridge-console link (ADR-0004).
//!
//! The console is the IPC **server** and the MCP bridge is the **client**. The
//! transport is kernel-authenticated to the same user with no TCP port at all:
//!
//! - on Windows, a per-user **named pipe** via
//!   `tokio::net::windows::named_pipe`, created with
//!   `reject_remote_clients` so no remote peer can connect and
//!   `first_pipe_instance` so a squatter cannot pre-create the name;
//! - on Unix, a **Unix domain socket** via [`tokio::net::UnixListener`], created
//!   `0600` inside a per-endpoint `0700` directory (see below).
//!
//! The two back-ends are `cfg`-gated behind one OS-neutral surface
//! ([`IpcEndpoint`], [`IpcListener`], [`IpcStream`], [`connect`]) so the rest of
//! the codebase never sees the platform split and a Windows -> Linux
//! cross-compile exercises the Unix path.
//!
//! # Unix same-user directory (ADR-0004)
//!
//! The socket lives inside a per-endpoint directory created atomically with mode
//! `0700`, rather than as a bare `0600` file in a shared runtime/temp directory.
//! The containing directory — not just the socket's own mode — enforces the
//! same-user guarantee, which closes two gaps: the brief window between `bind()`
//! and the socket's `chmod`, and the world-writable `temp_dir` fallback used
//! when `XDG_RUNTIME_DIR` is unset. An existing directory with any group/other
//! permission bit (for example one pre-created by a hostile local user in a
//! sticky `temp_dir`) is rejected, and a `0700` directory owned by someone else
//! is untraversable, so binding fails closed either way — a denial of service at
//! worst, never a cross-user endpoint.
//!
//! # Windows security-descriptor note (ADR-0004, threat model)
//!
//! A fully explicit same-user DACL on the pipe, and a connect-side check of the
//! pipe server's owner SID, both need raw Win32 calls and therefore `unsafe`,
//! which the workspace denies. This slice relies on the default pipe ACL plus
//! `reject_remote_clients(true)` and `first_pipe_instance(true)`; the
//! explicit-DACL hardening and the server-SID verification remain the ADR-0004
//! threat-model item and must fail closed if they cannot be applied.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A logical endpoint name shared by the console and its bridges. The concrete
/// pipe name (Windows) or socket path (Unix) is derived from it, so callers
/// never spell the OS-specific form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcEndpoint {
    name: String,
}

impl IpcEndpoint {
    /// The per-user default endpoint, derived from the current username so two
    /// users on one machine never share an endpoint. Bridges spawned by MCP
    /// clients and the console both resolve to this name.
    #[must_use]
    pub fn user_default() -> Self {
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".to_string());
        Self::named(format!("cretlink-{}", sanitize(&user)))
    }

    /// An explicitly named endpoint. Used to run multiple isolated endpoints
    /// (for example one per integration test). The name is sanitized to a
    /// filesystem- and pipe-safe token.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        let raw = name.into();
        let cleaned = sanitize(&raw);
        Self {
            name: if cleaned.is_empty() {
                "cretlink".to_string()
            } else {
                cleaned
            },
        }
    }

    /// The logical endpoint name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[cfg(windows)]
    fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\{}", self.name)
    }

    /// The per-endpoint directory that holds the socket. Created `0700` by the
    /// listener so the containing directory — not just the socket's own mode —
    /// enforces same-user access. This closes the create-then-chmod window and
    /// removes the world-writable-`temp_dir` exposure: even in a shared base
    /// (`temp_dir` when `XDG_RUNTIME_DIR` is unset), no other local user can
    /// traverse a `0700` directory to reach the socket inside it.
    #[cfg(unix)]
    fn socket_dir(&self) -> std::path::PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join(&self.name)
    }

    #[cfg(unix)]
    fn socket_path(&self) -> std::path::PathBuf {
        self.socket_dir().join("bridge.sock")
    }
}

/// Keeps an endpoint name to characters that are safe in both a pipe name and a
/// filename.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A connected same-user IPC stream (server or client end), OS-neutral.
#[derive(Debug)]
pub struct IpcStream {
    inner: imp::Stream,
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The console-side IPC listener. Accepts same-user bridge connections one at a
/// time; the console multiplexes them against its single active session.
#[derive(Debug)]
pub struct IpcListener {
    inner: imp::Listener,
}

impl IpcListener {
    /// Binds the listener at `endpoint`, failing closed if the pipe/socket
    /// cannot be created with the intended same-user restriction.
    ///
    /// # Errors
    ///
    /// Any OS error creating the pipe or socket (including a name already in
    /// use by another instance).
    pub fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
        Ok(Self {
            inner: imp::Listener::bind(endpoint)?,
        })
    }

    /// Accepts the next bridge connection.
    ///
    /// # Errors
    ///
    /// Any OS error while waiting for or accepting a connection.
    pub async fn accept(&mut self) -> io::Result<IpcStream> {
        Ok(IpcStream {
            inner: self.inner.accept().await?,
        })
    }
}

/// Connects to the console's IPC endpoint as a bridge client.
///
/// # Errors
///
/// Any OS error dialing the endpoint (for example the console is not running,
/// or the pipe/socket does not exist).
pub async fn connect(endpoint: &IpcEndpoint) -> io::Result<IpcStream> {
    Ok(IpcStream {
        inner: imp::connect(endpoint).await?,
    })
}

#[cfg(windows)]
mod imp {
    use super::IpcEndpoint;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// `ERROR_PIPE_BUSY`: all pipe instances are momentarily connected; the
    /// client retries. Hardcoded to avoid a `windows-sys` dependency.
    const ERROR_PIPE_BUSY: i32 = 231;

    #[derive(Debug)]
    pub(super) struct Listener {
        pipe_name: String,
        server: NamedPipeServer,
    }

    impl Listener {
        pub(super) fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
            let pipe_name = endpoint.pipe_name();
            // `first_pipe_instance(true)` fails closed if the name already
            // exists, so a squatter cannot pre-create the endpoint;
            // `reject_remote_clients(true)` bars any non-local peer.
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&pipe_name)?;
            Ok(Self { pipe_name, server })
        }

        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            // Wait for a client on the current instance, then swap in a fresh
            // instance for the next connection and hand off the connected one.
            self.server.connect().await?;
            let next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.pipe_name)?;
            let connected = std::mem::replace(&mut self.server, next);
            Ok(Stream::Server(connected))
        }
    }

    #[derive(Debug)]
    pub(super) enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(s) => Pin::new(s).poll_read(cx, buf),
                Self::Client(c) => Pin::new(c).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Server(s) => Pin::new(s).poll_write(cx, buf),
                Self::Client(c) => Pin::new(c).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(s) => Pin::new(s).poll_flush(cx),
                Self::Client(c) => Pin::new(c).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(s) => Pin::new(s).poll_shutdown(cx),
                Self::Client(c) => Pin::new(c).poll_shutdown(cx),
            }
        }
    }

    pub(super) async fn connect(endpoint: &IpcEndpoint) -> io::Result<Stream> {
        let pipe_name = endpoint.pipe_name();
        loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(client) => return Ok(Stream::Client(client)),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::IpcEndpoint;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::{UnixListener, UnixStream};

    #[derive(Debug)]
    pub(super) struct Listener {
        listener: UnixListener,
        path: PathBuf,
        dir: PathBuf,
    }

    impl Listener {
        pub(super) fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
            // Create (or validate) the per-endpoint 0700 directory *before*
            // binding, so the containing directory enforces same-user access
            // for the whole life of the socket — closing the window between
            // bind() and the socket's own chmod, and the world-writable-
            // temp_dir exposure when XDG_RUNTIME_DIR is unset.
            let dir = endpoint.socket_dir();
            ensure_private_dir(&dir)?;
            let path = endpoint.socket_path();
            // Remove a stale socket so a crashed prior console does not block
            // rebinding; a live one would already hold the name and this
            // reclaim is same-user only.
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            // Restrict the socket to the owner (0600) as defence in depth; the
            // 0700 parent directory is the primary same-user guard.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                listener,
                path,
                dir,
            })
        }

        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            let (stream, _addr) = self.listener.accept().await?;
            Ok(Stream(stream))
        }
    }

    /// Creates the per-endpoint directory `0700`, or validates an existing one,
    /// failing closed if it could expose the socket to another local user.
    ///
    /// The directory is created atomically with mode `0700` (subject to umask,
    /// which can only *remove* bits), so there is no window in which it is more
    /// permissive. If it already exists, it must be a directory with no group or
    /// other permission bits; a group- or world-accessible directory — for
    /// example one pre-created by a hostile local user in a sticky `temp_dir` —
    /// is rejected. A `0700` directory owned by another user is untraversable,
    /// so `bind()` would then fail closed on its own; either way no cross-user
    /// endpoint is ever created.
    fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let meta = std::fs::metadata(dir)?;
                if !meta.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "ipc endpoint directory path is not a directory",
                    ));
                }
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "ipc endpoint directory is group- or world-accessible",
                    ));
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Best-effort cleanup of the socket file and its now-empty
            // per-endpoint directory on shutdown.
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    #[derive(Debug)]
    pub(super) struct Stream(UnixStream);

    impl AsyncRead for Stream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    pub(super) async fn connect(endpoint: &IpcEndpoint) -> io::Result<Stream> {
        Ok(Stream(UnixStream::connect(endpoint.socket_path()).await?))
    }
}
