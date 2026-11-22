// conncap.rs — how many connections one address, and all addresses together, may hold open on a listening port.
// The ports a boot needs (HTTP for the boot scripts and the machines' own requests, iSCSI for their disks) are open to
// every host on the boot LAN before any check, and each open connection holds memory: buffers, and whatever part of
// a request has arrived. Without a limit one host holds as many as it likes, for as long as it likes, and the server
// runs out of memory or of file descriptors — for every machine. With it one address gets PER_IP at most (far above
// what a PC or an admin's browser uses), all of them together `total`, and a connection over the limit is closed at
// once (logged, not more than once a minute).
// - The server's own address (the maintenance VM behind QEMU's NAT, this server's own iSCSI logins) is not held to
//   the per-address limit.
// - `Capped` wraps an axum listener; `Conn` is its connection: the place in the count is given back when the
//   connection is dropped, however it ends.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct Open {
    by_ip: HashMap<IpAddr, usize>,
    all: usize,
    /// When a refusal was last logged.
    said: Option<Instant>,
}

pub struct Caps {
    /// What it limits, for the log ("web", "iSCSI").
    what: &'static str,
    per_ip: usize,
    total: usize,
    open: Mutex<Open>,
}

/// A connection's place in the count; given back when dropped.
pub struct Held(Arc<Caps>, IpAddr);

impl Drop for Held {
    fn drop(&mut self) {
        let mut o = self.0.open.lock();
        o.all = o.all.saturating_sub(1);
        if let Some(n) = o.by_ip.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                o.by_ip.remove(&self.1);
            }
        }
    }
}

impl Caps {
    pub(crate) fn new(what: &'static str, per_ip: usize, total: usize) -> Arc<Caps> {
        Arc::new(Caps {
            what,
            per_ip,
            total,
            open: Mutex::new(Open::default()),
        })
    }

    /// A place for a new connection from `ip`, or None: that address (or everyone together) has too many open.
    pub(crate) fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Held> {
        let ip = ip.to_canonical();
        let mut o = self.open.lock();
        let mine = o.by_ip.get(&ip).copied().unwrap_or(0);
        let why = if o.all >= self.total {
            Some(format!("{} connections are open", o.all))
        } else if mine >= self.per_ip && !ip.is_loopback() {
            Some(format!("it has {mine} open"))
        } else {
            None
        };
        if let Some(why) = why {
            let now = Instant::now();
            if o.said
                .is_none_or(|t| now.duration_since(t) > Duration::from_secs(60))
            {
                o.said = Some(now);
                tracing::warn!(
                    "{}: connection from {ip} closed — {why} (at most {} per address, {} in all)",
                    self.what,
                    self.per_ip,
                    self.total
                );
            }
            return None;
        }
        o.all += 1;
        *o.by_ip.entry(ip).or_default() += 1;
        Some(Held(self.clone(), ip))
    }

    #[cfg(test)]
    pub(crate) fn open(&self) -> (usize, usize) {
        let o = self.open.lock();
        (o.all, o.by_ip.len())
    }
}

/// A connection that holds its place in the count.
pub struct Conn<Io> {
    pub(crate) io: Io,
    _held: Held,
}

impl<Io> Conn<Io> {
    pub(crate) fn new(io: Io, held: Held) -> Conn<Io> {
        Conn { io, _held: held }
    }
}

impl<Io: AsyncRead + Unpin> AsyncRead for Conn<Io> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<Io: AsyncWrite + Unpin> AsyncWrite for Conn<Io> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

/// An axum listener whose connections are counted (`Caps`): one over the limit is closed as it is accepted.
pub(crate) struct Capped<L> {
    inner: L,
    caps: Arc<Caps>,
}

impl<L> Capped<L> {
    pub(crate) fn new(inner: L, caps: Arc<Caps>) -> Capped<L> {
        Capped { inner, caps }
    }
}

impl<L> axum::serve::Listener for Capped<L>
where
    L: axum::serve::Listener<Addr = SocketAddr>,
    L::Io: Unpin,
{
    type Io = Conn<L::Io>;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (io, addr) = self.inner.accept().await;
            if let Some(held) = self.caps.admit(addr.ip()) {
                return (Conn::new(io, held), addr);
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
#[path = "../tests/unit/conncap.rs"]
mod tests;
