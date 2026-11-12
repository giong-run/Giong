//! Addresses to connect to, resolved without blocking, and connecting to the
//! first of several addresses that answers.

use std::future::{poll_fn, Future};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use crate::net::dns;
use crate::time::{sleep, Instant};

/// Something [`TcpStream::connect`](crate::net::TcpStream::connect) and
/// [`lookup_host`] can turn into socket addresses: a `SocketAddr`, an
/// `(IpAddr, u16)` pair, a `"host:port"` string, a `(host, port)` pair or a
/// slice of addresses. Host names are resolved with the worker's
/// [`Resolver`](crate::net::Resolver), without blocking.
///
/// This trait is sealed: it cannot be implemented outside of giong.
pub trait ToSocketAddrs: sealed::Sealed {}

pub(crate) mod sealed {
    use super::*;

    /// What an address argument stands for.
    pub enum Target<'a> {
        Resolved(Vec<SocketAddr>),
        Host(&'a str, u16),
    }

    pub trait Sealed {
        fn target(&self) -> io::Result<Target<'_>>;
    }
}

use sealed::{Sealed, Target};

macro_rules! resolved {
    ($($t:ty),*) => {$(
        impl ToSocketAddrs for $t {}
        impl Sealed for $t {
            fn target(&self) -> io::Result<Target<'_>> {
                Ok(Target::Resolved(vec![SocketAddr::from(*self)]))
            }
        }
    )*};
}

resolved!(
    SocketAddr,
    SocketAddrV4,
    SocketAddrV6,
    (IpAddr, u16),
    (Ipv4Addr, u16),
    (Ipv6Addr, u16)
);

/// A host and a port: resolved at once if the host is an IP address.
fn host_port(host: &str, port: u16) -> Target<'_> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(ip) => Target::Resolved(vec![SocketAddr::new(ip, port)]),
        Err(_) => Target::Host(host, port),
    }
}

impl ToSocketAddrs for (&str, u16) {}
impl Sealed for (&str, u16) {
    fn target(&self) -> io::Result<Target<'_>> {
        Ok(host_port(self.0, self.1))
    }
}

impl ToSocketAddrs for (String, u16) {}
impl Sealed for (String, u16) {
    fn target(&self) -> io::Result<Target<'_>> {
        Ok(host_port(&self.0, self.1))
    }
}

impl ToSocketAddrs for str {}
impl Sealed for str {
    fn target(&self) -> io::Result<Target<'_>> {
        if let Ok(addr) = self.parse::<SocketAddr>() {
            return Ok(Target::Resolved(vec![addr]));
        }
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected \"host:port\", got {self:?}"),
            )
        };
        let (host, port) = self.rsplit_once(':').ok_or_else(invalid)?;
        let port = port.parse::<u16>().map_err(|_| invalid())?;
        Ok(host_port(host, port))
    }
}

impl ToSocketAddrs for String {}
impl Sealed for String {
    fn target(&self) -> io::Result<Target<'_>> {
        self.as_str().target()
    }
}

impl ToSocketAddrs for [SocketAddr] {}
impl Sealed for [SocketAddr] {
    fn target(&self) -> io::Result<Target<'_>> {
        Ok(Target::Resolved(self.to_vec()))
    }
}

impl ToSocketAddrs for Vec<SocketAddr> {}
impl Sealed for Vec<SocketAddr> {
    fn target(&self) -> io::Result<Target<'_>> {
        Ok(Target::Resolved(self.clone()))
    }
}

impl<const N: usize> ToSocketAddrs for [SocketAddr; N] {}
impl<const N: usize> Sealed for [SocketAddr; N] {
    fn target(&self) -> io::Result<Target<'_>> {
        Ok(Target::Resolved(self.to_vec()))
    }
}

impl<T: ToSocketAddrs + ?Sized> ToSocketAddrs for &T {}
impl<T: Sealed + ?Sized> Sealed for &T {
    fn target(&self) -> io::Result<Target<'_>> {
        (**self).target()
    }
}

/// Resolves `addr` to socket addresses without blocking the worker: IP
/// addresses are returned as they are, host names are looked up in
/// `/etc/hosts` and then with DNS (see [`Resolver`](crate::net::Resolver)).
///
/// ```no_run
/// # giong::block_on(async {
/// for addr in giong::net::lookup_host("example.com:443").await? {
///     println!("{addr}");
/// }
/// # Ok::<_, std::io::Error>(())
/// # }).unwrap();
/// ```
pub async fn lookup_host<A: ToSocketAddrs>(addr: A) -> io::Result<Vec<SocketAddr>> {
    match addr.target()? {
        Target::Resolved(addrs) => Ok(addrs),
        Target::Host(host, port) => {
            let resolver = dns::system_resolver()?;
            let ips = resolver.lookup_ip(host).await?;
            Ok(ips
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect())
        }
    }
}

/// How long one connection attempt gets before the next address is tried
/// alongside it (RFC 8305, "Happy Eyeballs").
pub(crate) const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Orders addresses for connecting: alternating families, starting with the
/// family of the first address (RFC 8305 §4).
pub(crate) fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let first_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (mut a, mut b): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|x| x.is_ipv6() == first_v6);
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.drain(..), b.drain(..));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return out,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
}

/// Connects to the first address that answers. The first attempt starts at
/// once; while it is still pending, the next one starts after
/// [`ATTEMPT_DELAY`], or as soon as an attempt fails. The first connection
/// made wins and the other attempts are dropped (and their sockets closed).
pub(crate) async fn connect_first<S, F, Fut>(addrs: Vec<SocketAddr>, connect: F) -> io::Result<S>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut queue = interleave(addrs).into_iter();
    let Some(first) = queue.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any address",
        ));
    };
    let mut running: Vec<Pin<Box<Fut>>> = vec![Box::pin(connect(first))];
    let mut timer = sleep(ATTEMPT_DELAY);
    let mut last_err = None;
    poll_fn(|cx| loop {
        let mut start_next = false;
        let mut i = 0;
        while i < running.len() {
            match running[i].as_mut().poll(cx) {
                Poll::Ready(Ok(stream)) => return Poll::Ready(Ok(stream)),
                Poll::Ready(Err(e)) => {
                    last_err = Some(e);
                    drop(running.swap_remove(i));
                    start_next = true;
                }
                Poll::Pending => i += 1,
            }
        }
        if !start_next && Pin::new(&mut timer).poll(cx).is_ready() {
            start_next = true;
        }
        if start_next {
            if let Some(next) = queue.next() {
                running.push(Box::pin(connect(next)));
                timer.reset(Instant::now() + ATTEMPT_DELAY);
                continue;
            }
        }
        if running.is_empty() {
            return Poll::Ready(Err(last_err.take().expect("an attempt failed")));
        }
        return Poll::Pending;
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(t: &(impl ToSocketAddrs + ?Sized)) -> String {
        match t.target() {
            Ok(Target::Resolved(a)) => format!("{a:?}"),
            Ok(Target::Host(h, p)) => format!("host {h} {p}"),
            Err(e) => format!("error {:?}", e.kind()),
        }
    }

    #[test]
    fn address_arguments_are_split_into_ips_and_names() {
        assert_eq!(target("127.0.0.1:80"), "[127.0.0.1:80]");
        assert_eq!(target("[::1]:443"), "[[::1]:443]");
        assert_eq!(target("db.internal:5432"), "host db.internal 5432");
        assert_eq!(
            target(&String::from("localhost:6379")),
            "host localhost 6379"
        );
        assert_eq!(target(&("example.com", 443)), "host example.com 443");
        assert_eq!(target(&("10.0.0.1", 1)), "[10.0.0.1:1]");
        assert_eq!(target(&("[::1]", 1)), "[[::1]:1]");
        assert_eq!(target(&(Ipv4Addr::LOCALHOST, 9)), "[127.0.0.1:9]");
        assert_eq!(target("no-port"), "error InvalidInput");
        assert_eq!(target("host:99999"), "error InvalidInput");
        let list: [SocketAddr; 2] = ["1.1.1.1:1".parse().unwrap(), "[::2]:2".parse().unwrap()];
        assert_eq!(target(&list), "[1.1.1.1:1, [::2]:2]");
        assert_eq!(target(&list[..]), "[1.1.1.1:1, [::2]:2]");
    }

    #[test]
    fn families_alternate_starting_with_the_first() {
        let v6: Vec<SocketAddr> = ["[::1]:1", "[::2]:1"].map(|a| a.parse().unwrap()).into();
        let v4: Vec<SocketAddr> = ["1.0.0.1:1", "1.0.0.2:1", "1.0.0.3:1"]
            .map(|a| a.parse().unwrap())
            .into();
        let mixed = [v6.clone(), v4.clone()].concat();
        assert_eq!(interleave(mixed), [v6[0], v4[0], v6[1], v4[1], v4[2]]);
        let mixed = [v4.clone(), v6.clone()].concat();
        assert_eq!(interleave(mixed), [v4[0], v6[0], v4[1], v6[1], v4[2]]);
        assert_eq!(interleave(v4.clone()), v4);
    }
}
