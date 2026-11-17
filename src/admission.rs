//! Bounded admission shared by MQTT TCP, TLS and WebSocket listeners.
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_KEYS: usize = 4096;
const IDLE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rate {
    pub burst: u32,
    pub refill: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectLimits {
    pub global: Rate,
    pub source: Rate,
    pub account: Rate,
    pub pending: usize,
    pub pending_per_ip: usize,
}

impl Default for ConnectLimits {
    fn default() -> Self {
        Self {
            global: Rate {
                burst: 20,
                refill: Duration::from_millis(200),
            },
            source: Rate {
                burst: 10,
                refill: Duration::from_secs(1),
            },
            account: Rate {
                burst: 10,
                refill: Duration::from_secs(5),
            },
            pending: 32,
            pending_per_ip: 2,
        }
    }
}

impl ConnectLimits {
    pub fn validate(&self) -> Result<()> {
        for rate in [&self.global, &self.source, &self.account] {
            ensure!(
                (1..=10_000).contains(&rate.burst),
                "MQTT connection burst must be in 1..=10000"
            );
            ensure!(
                (Duration::from_millis(1)..=Duration::from_secs(3600)).contains(&rate.refill),
                "MQTT connection refill must be in 1ms..=1h"
            );
        }
        ensure!(
            (1..=256).contains(&self.pending)
                && self.pending_per_ip > 0
                && self.pending_per_ip <= self.pending,
            "Invalid MQTT pending handshake limits"
        );
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("MQTT connection admission limited: {0}")]
pub struct AdmissionRejected(&'static str);

struct Bucket {
    tokens: f64,
    updated: Instant,
    last_used: Instant,
}
impl Bucket {
    fn new(rule: &Rate, now: Instant) -> Self {
        Self {
            tokens: f64::from(rule.burst),
            updated: now,
            last_used: now,
        }
    }
    fn refill(&mut self, rule: &Rate, now: Instant) {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64()
                / rule.refill.as_secs_f64())
        .min(f64::from(rule.burst));
        self.updated = now;
    }
    fn take(&mut self, now: Instant) {
        self.tokens -= 1.;
        self.last_used = now;
    }
}

struct Source {
    bucket: Bucket,
    pending: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Principal {
    Account(Uuid),
    User(Uuid),
}
struct State {
    global: Bucket,
    sources: HashMap<IpAddr, Source>,
    accounts: HashMap<Principal, Bucket>,
    pending: usize,
    cleaned: Instant,
}
impl State {
    fn cleanup(&mut self, rules: &ConnectLimits, now: Instant) {
        if now.saturating_duration_since(self.cleaned) < Duration::from_secs(1) {
            return;
        }
        self.cleaned = now;
        self.sources.retain(|_, entry| {
            entry.bucket.refill(&rules.source, now);
            entry.pending > 0
                || now.saturating_duration_since(entry.bucket.last_used) < IDLE_TTL
                || entry.bucket.tokens < f64::from(rules.source.burst)
        });
        self.accounts.retain(|_, entry| {
            entry.refill(&rules.account, now);
            now.saturating_duration_since(entry.last_used) < IDLE_TTL
                || entry.tokens < f64::from(rules.account.burst)
        });
    }
}

struct Inner {
    rules: ConnectLimits,
    state: Mutex<State>,
    rate_rejections: AtomicU64,
    pending_rejections: AtomicU64,
    account_rejections: AtomicU64,
}
#[derive(Clone)]
pub struct ConnectionControl(Arc<Inner>);

/// A pending handshake slot; dropped immediately after CONNECT is accepted.
pub struct ConnectionAdmission {
    control: ConnectionControl,
    ip: IpAddr,
}
impl Drop for ConnectionAdmission {
    fn drop(&mut self) {
        let mut state = self
            .control
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.pending -= 1;
        if let Some(source) = state.sources.get_mut(&self.ip) {
            source.pending -= 1;
        }
    }
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

impl ConnectionControl {
    pub fn new(rules: ConnectLimits) -> Result<Self> {
        rules.validate()?;
        let now = Instant::now();
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                global: Bucket::new(&rules.global, now),
                sources: HashMap::new(),
                accounts: HashMap::new(),
                pending: 0,
                cleaned: now,
            }),
            rules,
            rate_rejections: AtomicU64::new(0),
            pending_rejections: AtomicU64::new(0),
            account_rejections: AtomicU64::new(0),
        })))
    }
    pub fn admit(&self, ip: IpAddr) -> Result<ConnectionAdmission, AdmissionRejected> {
        self.admit_at(ip, Instant::now())
    }
    fn admit_at(&self, ip: IpAddr, now: Instant) -> Result<ConnectionAdmission, AdmissionRejected> {
        let ip = normalize(ip);
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let rules = &self.0.rules;
        state.cleanup(rules, now);
        if state.pending >= rules.pending
            || state
                .sources
                .get(&ip)
                .is_some_and(|s| s.pending >= rules.pending_per_ip)
        {
            self.0.pending_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("pending handshakes"));
        }
        state.global.refill(&rules.global, now);
        if state.global.tokens < 1. {
            self.0.rate_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("global rate"));
        }
        if state.sources.len() >= MAX_KEYS && !state.sources.contains_key(&ip) {
            self.0.rate_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("source tracking capacity"));
        }
        let source = state.sources.entry(ip).or_insert_with(|| Source {
            bucket: Bucket::new(&rules.source, now),
            pending: 0,
        });
        source.bucket.refill(&rules.source, now);
        if source.bucket.tokens < 1. {
            self.0.rate_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("source rate"));
        }
        source.bucket.take(now);
        source.pending += 1;
        state.global.take(now);
        state.pending += 1;
        Ok(ConnectionAdmission {
            control: self.clone(),
            ip,
        })
    }
    pub fn owns(&self, admission: &ConnectionAdmission, ip: IpAddr) -> bool {
        Arc::ptr_eq(&self.0, &admission.control.0) && admission.ip == normalize(ip)
    }
    pub fn account(&self, owner: Option<Uuid>, user: Uuid) -> Result<(), AdmissionRejected> {
        self.account_at(owner, user, Instant::now())
    }
    fn account_at(
        &self,
        owner: Option<Uuid>,
        user: Uuid,
        now: Instant,
    ) -> Result<(), AdmissionRejected> {
        let key = owner
            .map(Principal::Account)
            .unwrap_or(Principal::User(user));
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cleanup(&self.0.rules, now);
        if state.accounts.len() >= MAX_KEYS && !state.accounts.contains_key(&key) {
            self.0.account_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("account tracking capacity"));
        }
        let rule = &self.0.rules.account;
        let bucket = state
            .accounts
            .entry(key)
            .or_insert_with(|| Bucket::new(rule, now));
        bucket.refill(rule, now);
        if bucket.tokens < 1. {
            self.0.account_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionRejected("account rate"));
        }
        bucket.take(now);
        Ok(())
    }
    pub fn rejections(&self) -> (u64, u64, u64) {
        (
            self.0.rate_rejections.load(Ordering::Relaxed),
            self.0.pending_rejections.load(Ordering::Relaxed),
            self.0.account_rejections.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rules() -> ConnectLimits {
        ConnectLimits {
            global: Rate {
                burst: 20,
                refill: Duration::from_secs(1),
            },
            source: Rate {
                burst: 2,
                refill: Duration::from_secs(1),
            },
            account: Rate {
                burst: 1,
                refill: Duration::from_secs(1),
            },
            pending: 8,
            pending_per_ip: 2,
        }
    }
    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, last])
    }

    #[test]
    fn source_bursts_are_bounded_and_refill_without_penalizing_other_sources() {
        let gate = ConnectionControl::new(rules()).unwrap();
        let now = Instant::now();
        drop(gate.admit_at(ip(1), now).unwrap());
        drop(gate.admit_at(ip(1), now).unwrap());
        for _ in 0..100 {
            assert!(gate.admit_at(ip(1), now).is_err());
        }
        assert!(gate.admit_at(ip(2), now).is_ok());
        assert!(gate.admit_at(ip(1), now + Duration::from_secs(1)).is_ok());
    }
    #[test]
    fn ipv4_mapped_addresses_share_the_source_budget() {
        let mut limits = rules();
        limits.source.burst = 1;
        let gate = ConnectionControl::new(limits).unwrap();
        let now = Instant::now();
        drop(gate.admit_at(ip(1), now).unwrap());
        assert!(
            gate.admit_at("::ffff:192.0.2.1".parse().unwrap(), now)
                .is_err()
        );
    }
    #[test]
    fn account_budget_spans_mqtt_users_and_client_ids() {
        let gate = ConnectionControl::new(rules()).unwrap();
        let now = Instant::now();
        let owner = Uuid::new_v4();
        gate.account_at(Some(owner), Uuid::new_v4(), now).unwrap();
        assert!(gate.account_at(Some(owner), Uuid::new_v4(), now).is_err());
        assert!(
            gate.account_at(Some(Uuid::new_v4()), Uuid::new_v4(), now)
                .is_ok()
        );
        assert!(gate.account_at(None, owner, now).is_ok());
        assert!(
            gate.account_at(Some(owner), Uuid::new_v4(), now + Duration::from_secs(1))
                .is_ok()
        );
    }
    #[test]
    fn pending_slots_are_scoped_and_released_on_failed_or_cancelled_handshakes() {
        let mut limits = rules();
        limits.pending = 2;
        limits.pending_per_ip = 1;
        let gate = ConnectionControl::new(limits).unwrap();
        let first = gate.admit(ip(1)).unwrap();
        assert!(gate.admit(ip(1)).is_err());
        let second = gate.admit(ip(2)).unwrap();
        assert!(gate.admit(ip(3)).is_err());
        drop(first);
        let third = gate.admit(ip(3)).unwrap();
        drop(second);
        drop(third);
        assert_eq!(gate.0.state.lock().unwrap().pending, 0);
    }
    #[test]
    fn admission_cannot_be_reused_for_another_control_or_source() {
        let gate = ConnectionControl::new(rules()).unwrap();
        let other = ConnectionControl::new(rules()).unwrap();
        let ticket = gate.admit(ip(1)).unwrap();
        assert!(gate.owns(&ticket, ip(1)));
        assert!(!gate.owns(&ticket, ip(2)));
        assert!(!other.owns(&ticket, ip(1)));
    }
    #[test]
    fn source_and_account_tracking_stay_bounded_and_reclaim_idle_entries() {
        let mut limits = rules();
        limits.global.burst = 10_000;
        limits.account.refill = Duration::from_millis(1);
        let gate = ConnectionControl::new(limits).unwrap();
        let now = Instant::now();
        for n in 1..=MAX_KEYS {
            drop(
                gate.admit_at(IpAddr::V4(std::net::Ipv4Addr::from(n as u32)), now)
                    .unwrap(),
            );
            gate.account_at(Some(Uuid::from_u128(n as u128)), Uuid::nil(), now)
                .unwrap();
        }
        assert!(gate.admit_at(ip(9), now).is_err());
        assert!(
            gate.account_at(Some(Uuid::new_v4()), Uuid::nil(), now)
                .is_err()
        );
        let later = now + IDLE_TTL + Duration::from_secs(1);
        assert!(gate.admit_at(ip(9), later).is_ok());
        assert!(
            gate.account_at(Some(Uuid::new_v4()), Uuid::nil(), later)
                .is_ok()
        );
        let state = gate.0.state.lock().unwrap();
        assert_eq!(state.sources.len(), 1);
        assert_eq!(state.accounts.len(), 1);
    }
    #[test]
    fn invalid_admission_limits_are_rejected() {
        let mut limits = rules();
        limits.account.refill = Duration::ZERO;
        assert!(limits.validate().is_err());
        limits = rules();
        limits.pending_per_ip = limits.pending + 1;
        assert!(limits.validate().is_err());
    }
}
