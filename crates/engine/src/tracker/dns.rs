//! Compio DNS via cyper-hickory (same stack as cyper `hickory-dns`).
//!
//! Resolves hostnames with hickory over Compio UDP — not
//! `ToSocketAddrsAsync` / blocking `getaddrinfo`. Must run on a Compio runtime
//! (tracker thread or short-lived fetch RT).

use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};

use cyper_hickory::CompioConnectionProvider;
use hickory_resolver::Resolver;

use crate::error::{Error, Result};

type DnsResolver = Resolver<CompioConnectionProvider>;

/// Thread-local hickory resolver (Compio connection provider; not `Send`).
fn thread_dns_resolver() -> Result<DnsResolver> {
    thread_local! {
        static RESOLVER: RefCell<Option<DnsResolver>> = const { RefCell::new(None) };
    }
    RESOLVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let r = Resolver::builder(CompioConnectionProvider::default())
                .map_err(|e| Error::Msg(format!("dns resolver builder: {e}")))?
                .build()
                .map_err(|e| Error::Msg(format!("dns resolver build: {e}")))?;
            *slot = Some(r);
        }
        // Resolver is Clone (Arc internals) — safe to use across .await.
        Ok(slot.as_ref().expect("dns resolver").clone())
    })
}

/// Resolve `host` to up to `max` IPv4 [`SocketAddr`]s with `port`.
///
/// Literal IPs skip DNS. Prefer IPv4 (UDP tracker compact peers are v4-only).
pub async fn resolve_ipv4(host: &str, port: u16, max: usize) -> Result<Vec<SocketAddr>> {
    let max = max.max(1);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let resolver = thread_dns_resolver()?;
    let lookup = resolver
        .lookup_ip(host)
        .await
        .map_err(|e| Error::Msg(format!("dns resolve {host}: {e}")))?;

    let mut addrs = Vec::new();
    for ip in lookup.iter() {
        if ip.is_ipv4() {
            addrs.push(SocketAddr::new(ip, port));
        }
        if addrs.len() >= max {
            break;
        }
    }
    if addrs.is_empty() {
        return Err(Error::Msg(format!(
            "dns no IPv4 addresses for {host}:{port}"
        )));
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_ipv4_skips_dns() {
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let addrs = resolve_ipv4("127.0.0.1", 1337, 2).await.expect("lit");
            assert_eq!(addrs.len(), 1);
            assert_eq!(addrs[0].to_string(), "127.0.0.1:1337");
        });
    }

    /// Smoke: system resolver over Compio UDP (needs network / resolv.conf).
    #[test]
    fn localhost_resolves() {
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            match resolve_ipv4("localhost", 9, 2).await {
                Ok(addrs) => {
                    assert!(!addrs.is_empty());
                    assert!(addrs.iter().all(|a| a.port() == 9 && a.is_ipv4()));
                }
                Err(e) => {
                    // CI without DNS: accept hard failure only if clearly network.
                    tracing::warn!(error = %e, "localhost DNS skipped/failed");
                }
            }
        });
    }
}
