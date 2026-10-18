//! A per-address limit on connection attempts before the handshake.
//!
//! The handshake signs with ML-DSA-65, which is expensive on purpose, so an
//! unauthenticated caller must not be able to keep many of them running by
//! opening connections in a loop. The accept loops of the server
//! ([`crate::server`]) and of cmdterm-web's link port check [`Attempts`]
//! before they hand a socket to a thread: a sliding window of attempts per
//! address, and a bounded number of remembered addresses.
//!
//! IPv6 addresses count per /64, the block one host or one customer usually
//! gets, so walking through the addresses of one network does not reset
//! the limit. A connection that logs in gives its attempt back
//! ([`Attempts::forgive`]): only callers that hold an authorized key can do
//! that, and `exec --direct` opens one connection per command.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far back attempts count.
const WINDOW: Duration = Duration::from_secs(60);
/// Attempts one address may make per window.
const MAX_PER_ADDRESS: usize = 8;
/// Addresses remembered at once. When a new one arrives and the map is
/// full, addresses whose window has passed are dropped first, else the one
/// least recently seen, so an attacker who fills the map cannot lock new
/// clients out.
const MAX_ADDRESSES: usize = 10_000;

/// Sliding-window attempt counter, per address.
#[derive(Default)]
pub struct Attempts {
    map: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl Attempts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `ip` may start another connection now. An allowed attempt
    /// is recorded.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let ip = counted(ip);
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(&ip) && map.len() >= MAX_ADDRESSES {
            // Make room for the new address instead of refusing it: the
            // entries whose window has passed first, then the one least
            // recently seen. Only a full map pays this scan.
            map.retain(|_, times| {
                times
                    .last()
                    .is_some_and(|t| now.duration_since(*t) < WINDOW)
            });
            if map.len() >= MAX_ADDRESSES {
                let oldest = map
                    .iter()
                    .min_by_key(|(_, times)| times.last().copied())
                    .map(|(oldest, _)| *oldest);
                if let Some(oldest) = oldest {
                    map.remove(&oldest);
                }
            }
        }
        let times = map.entry(ip).or_default();
        times.retain(|t| now.duration_since(*t) < WINDOW);
        if times.len() >= MAX_PER_ADDRESS {
            return false;
        }
        times.push(now);
        true
    }

    /// Takes back the newest attempt of `ip`, after its connection logged
    /// in with an authorized key.
    pub fn forgive(&self, ip: IpAddr) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(times) = map.get_mut(&counted(ip)) {
            times.pop();
        }
    }
}

/// The address `ip` counts under: IPv4 as it is, also when mapped into
/// IPv6, and other IPv6 addresses by their /64.
fn counted(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let bits = u128::from(v6) & !((1u128 << 64) - 1);
                IpAddr::V6(Ipv6Addr::from(bits))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn eight_attempts_per_minute_then_refused_until_the_window_slides() {
        let attempts = Attempts::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        for _ in 0..MAX_PER_ADDRESS {
            assert!(attempts.allow(ip));
        }
        assert!(!attempts.allow(ip));
        // Another address is unaffected.
        assert!(attempts.allow(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))));
        // A stale entry ages out: push the window forward by hand.
        {
            let mut map = attempts.map.lock().unwrap();
            let times = map.get_mut(&ip).unwrap();
            for t in times.iter_mut() {
                *t = now_minus(WINDOW + Duration::from_secs(1));
            }
        }
        assert!(attempts.allow(ip));
    }

    #[test]
    fn ipv6_counts_per_64_and_logins_give_their_attempt_back() {
        let attempts = Attempts::new();
        let host = |last: u16| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, last));
        for i in 0..MAX_PER_ADDRESS as u16 {
            assert!(attempts.allow(host(i)));
        }
        assert!(
            !attempts.allow(host(999)),
            "another address of the same /64"
        );
        let neighbor = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1));
        assert!(attempts.allow(neighbor), "the next /64 has its own limit");

        // A mapped IPv4 address counts as that IPv4 address.
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        for _ in 0..MAX_PER_ADDRESS {
            assert!(attempts.allow(v4));
        }
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert!(!attempts.allow(mapped));

        // Commands that each log in on their own connection keep working.
        let client = IpAddr::V4(Ipv4Addr::new(10, 9, 0, 1));
        for _ in 0..MAX_PER_ADDRESS * 3 {
            assert!(attempts.allow(client));
            attempts.forgive(client);
        }
    }

    fn now_minus(ago: Duration) -> Instant {
        Instant::now().checked_sub(ago).unwrap()
    }

    #[test]
    fn a_full_map_makes_room_for_a_new_address() {
        let attempts = Attempts::new();
        for i in 0..MAX_ADDRESSES {
            let ip = IpAddr::V4(Ipv4Addr::new(10, 1, (i >> 8) as u8, i as u8));
            assert!(attempts.allow(ip));
        }
        let fresh = IpAddr::V4(Ipv4Addr::new(10, 2, 0, 1));
        assert!(attempts.allow(fresh), "a full map does not lock out");
        let map = attempts.map.lock().unwrap();
        assert_eq!(map.len(), MAX_ADDRESSES);
        drop(map);
        assert!(attempts.allow(fresh));
    }
}
