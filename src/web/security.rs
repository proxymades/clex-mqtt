use super::*;
use std::{collections::HashMap, net::IpAddr};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_ADDRESSES: usize = 1024;
const MAX_FAILURE_EVENTS: u32 = 120;

struct Window {
    started: Instant,
    count: u32,
    reported: bool,
}

pub(super) struct LoginSecurity {
    addresses: HashMap<IpAddr, Window>,
    events_started: Instant,
    events: u32,
    cleanup: Option<Instant>,
}

impl Default for LoginSecurity {
    fn default() -> Self {
        Self {
            addresses: HashMap::new(),
            events_started: Instant::now(),
            events: 0,
            cleanup: None,
        }
    }
}

impl LoginSecurity {
    // The boolean records only the first rejection for an address in this window.
    pub(super) fn attempt(&mut self, ip: IpAddr, now: Instant) -> Result<(), bool> {
        self.addresses
            .retain(|_, entry| now.duration_since(entry.started) < WINDOW);
        if self.addresses.len() >= MAX_ADDRESSES && !self.addresses.contains_key(&ip) {
            return Err(true);
        }
        let entry = self.addresses.entry(ip).or_insert(Window {
            started: now,
            count: 0,
            reported: false,
        });
        if entry.count >= 10 {
            let report = !entry.reported;
            entry.reported = true;
            return Err(report);
        }
        entry.count += 1;
        Ok(())
    }

    pub(super) fn allow_event(&mut self, now: Instant) -> bool {
        if now.duration_since(self.events_started) >= WINDOW {
            self.events_started = now;
            self.events = 0;
        }
        if self.events >= MAX_FAILURE_EVENTS {
            return false;
        }
        self.events += 1;
        true
    }

    fn cleanup_due(&mut self, now: Instant) -> bool {
        if self
            .cleanup
            .is_some_and(|last| now.duration_since(last) < WINDOW)
        {
            return false;
        }
        self.cleanup = Some(now);
        true
    }
}

pub(super) fn client_ip(
    peer: SocketAddr,
    headers: &HeaderMap,
    trusted: &[IpAddr],
) -> ApiResult<IpAddr> {
    let peer_ip = normalize_ip(peer.ip());
    if !trusted.contains(&peer_ip) {
        return Ok(peer_ip);
    }
    let mut values = headers.get_all("x-real-ip").iter();
    let value = values
        .next()
        .ok_or_else(|| ApiError::bad("Прокси не передал IP клиента."))?;
    if values.next().is_some() {
        return Err(ApiError::bad("Прокси передал несколько IP клиента."));
    }
    value
        .to_str()
        .ok()
        .and_then(|ip| ip.parse().ok())
        .map(normalize_ip)
        .ok_or_else(|| ApiError::bad("Прокси передал некорректный IP клиента."))
}

pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

pub(super) fn limited_response(seconds: &'static str) -> Response {
    let mut response = ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "LOGIN_LIMIT",
        "Слишком много попыток. Повторите вход позже.",
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static(seconds));
    response
}

pub(super) async fn write_login(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    username: &str,
    ip: IpAddr,
    result: &str,
    reason: Option<&str>,
) -> ApiResult<()> {
    let action = match result {
        "success" => "admin.login",
        "limited" => "admin.login_limited",
        _ => "admin.login_failed",
    };
    let actor: String = username
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    sqlx::query("INSERT INTO audit_events (actor,action,details) VALUES ($1,$2,$3)")
        .bind(actor)
        .bind(action)
        .bind(json!({"ip":ip.to_string(),"result":result,"reason":reason}))
        .execute(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(())
}

pub(super) async fn failure(
    state: &WebState,
    username: &str,
    ip: IpAddr,
    result: &str,
    reason: &str,
) {
    if !state
        .login_security
        .lock()
        .await
        .allow_event(Instant::now())
    {
        return;
    }
    let operation = async {
        let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
        write_login(&mut tx, username, ip, result, Some(reason)).await?;
        tx.commit().await.map_err(|_| ApiError::internal())
    }
    .await;
    if operation.is_err() {
        tracing::warn!("Could not persist an administrator login audit event");
    }
    prune(state).await;
}

pub(super) async fn prune(state: &WebState) {
    if !state
        .login_security
        .lock()
        .await
        .cleanup_due(Instant::now())
    {
        return;
    }
    // Retain ordinary administrative actions. Only authentication events expire.
    let operation = async {
        let mut tx = state.pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '2s'").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM audit_events WHERE action IN ('admin.login','admin.login_failed','admin.login_limited','account.login','account.login_failed','account.login_limited') AND created_at < now()-interval '30 days'")
            .execute(&mut *tx).await?;
        sqlx::query("DELETE FROM audit_events WHERE action IN ('admin.login','admin.login_failed','admin.login_limited','account.login','account.login_failed','account.login_limited') AND id < (SELECT id FROM audit_events WHERE action IN ('admin.login','admin.login_failed','admin.login_limited','account.login','account.login_failed','account.login_limited') ORDER BY id DESC OFFSET 9999 LIMIT 1)")
            .execute(&mut *tx).await?;
        tx.commit().await
    }.await;
    if operation.is_err() {
        tracing::warn!("Could not prune administrator login audit events");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_headers_require_exact_trust_and_one_ip() {
        let peer = "127.0.0.1:1000".parse().unwrap();
        let trusted = ["127.0.0.1".parse().unwrap()];
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("192.0.2.1"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.99"));
        assert_eq!(
            client_ip(peer, &headers, &[]).unwrap().to_string(),
            "127.0.0.1"
        );
        assert_eq!(
            client_ip(peer, &headers, &trusted).unwrap().to_string(),
            "192.0.2.1"
        );
        for invalid in ["192.0.2.1, 192.0.2.2", "192.0.2.1:8000", "unknown", ""] {
            headers.insert("x-real-ip", HeaderValue::from_str(invalid).unwrap());
            assert!(client_ip(peer, &headers, &trusted).is_err());
        }
        headers.insert("x-real-ip", HeaderValue::from_static("::ffff:192.0.2.1"));
        assert_eq!(
            client_ip(peer, &headers, &trusted).unwrap().to_string(),
            "192.0.2.1"
        );
        headers.append("x-real-ip", HeaderValue::from_static("192.0.2.2"));
        assert!(client_ip(peer, &headers, &trusted).is_err());
        headers.clear();
        assert!(client_ip(peer, &headers, &trusted).is_err());
    }

    #[test]
    fn limits_expire_isolate_addresses_and_bound_memory_and_logging() {
        let start = Instant::now();
        let ip = "192.0.2.1".parse().unwrap();
        let mut security = LoginSecurity::default();
        for _ in 0..10 {
            assert!(security.attempt(ip, start).is_ok());
        }
        assert_eq!(security.attempt(ip, start), Err(true));
        assert_eq!(security.attempt(ip, start), Err(false));
        assert!(
            security
                .attempt("192.0.2.2".parse().unwrap(), start)
                .is_ok()
        );
        assert!(security.attempt(ip, start + WINDOW).is_ok());
        let mut security = LoginSecurity::default();
        for i in 0..MAX_ADDRESSES as u32 {
            assert!(
                security
                    .attempt(IpAddr::V4((0xc0000000 + i).into()), start)
                    .is_ok()
            );
        }
        assert!(
            security
                .attempt("198.51.100.1".parse().unwrap(), start)
                .is_err()
        );
        assert_eq!(security.addresses.len(), MAX_ADDRESSES);
        let now = Instant::now();
        for _ in 0..MAX_FAILURE_EVENTS {
            assert!(security.allow_event(now));
        }
        assert!(!security.allow_event(now));
        assert!(security.allow_event(now + WINDOW));
    }
}
