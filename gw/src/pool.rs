//! Accounts per provider: which one serves a request, and which are cooling down.

use crate::provider::Failure;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Sessions kept before the oldest is dropped.
const MAX_SESSIONS: usize = 10_000;
/// Cooldown for a rate limit that came without `Retry-After`.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
/// Cooldown for an exhausted plan quota with no reset time.
const QUOTA_COOLDOWN: Duration = Duration::from_secs(60 * 60);
/// Cooldown after a rejected credential: long enough to stop a retry storm, short enough to recover.
const AUTH_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Why an account is cooling down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Provider said slow down.
    RateLimited,
    /// Plan quota is used up.
    Quota,
    /// Credential was rejected.
    Auth,
}

/// What one attempt on an account means for that account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The account worked.
    Success,
    /// 429; cool it down for `retry_after` or a minute.
    RateLimited { retry_after: Option<Duration> },
    /// Quota used up; cool it down until `reset_after` or an hour.
    Quota { reset_after: Option<Duration> },
    /// Credential rejected; cool it down for five minutes.
    Auth,
}

impl Outcome {
    /// Account state implied by a failure; `None` when the account is not to blame.
    pub fn from_failure(failure: &Failure) -> Option<Outcome> {
        match failure {
            Failure::RateLimited { retry_after } => Some(Outcome::RateLimited {
                retry_after: *retry_after,
            }),
            Failure::Quota { reset_after } => Some(Outcome::Quota {
                reset_after: *reset_after,
            }),
            Failure::Auth => Some(Outcome::Auth),
            Failure::Retryable | Failure::Fatal { .. } => None,
        }
    }
}

struct AccountState {
    id: String,
    paused: bool,
    cooldown: Option<(Instant, Reason)>,
    /// Assignment counter value of the last pick; the smallest value is picked next.
    assigned: u64,
}

impl AccountState {
    /// Whether the account may serve a request now; expired cooldowns are cleared by the caller.
    fn usable(&self) -> bool {
        !self.paused && self.cooldown.is_none()
    }
}

struct Session {
    /// Provider to account, one entry per provider the session has used.
    accounts: Vec<(String, String)>,
    /// Combo to the target index the session settled on, and how many requests kept it.
    targets: Vec<(String, usize, u16)>,
    /// Insertion order; an entry is stale once a newer session reuses the same name.
    seq: u64,
}

/// Usable accounts per provider, plus what each session is using.
#[derive(Default)]
pub struct Pool {
    providers: HashMap<String, Vec<AccountState>>,
    sessions: HashMap<String, Session>,
    /// Session insertions in order, oldest first, for bounded memory.
    order: VecDeque<(u64, String)>,
    /// Bumped for every assignment and every session insertion.
    counter: u64,
    /// Bumped for every new session that needs a rotating start (combos use it).
    rotation: u64,
}

impl Pool {
    /// An empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an account; adding the same id twice keeps the first state.
    pub fn add(&mut self, provider: &str, account: &str) {
        let states = self.providers.entry(provider.to_owned()).or_default();
        if states.iter().any(|state| state.id == account) {
            return;
        }
        states.push(AccountState {
            id: account.to_owned(),
            paused: false,
            cooldown: None,
            assigned: 0,
        });
    }

    /// Pause or resume an account; unknown accounts are ignored.
    pub fn set_paused(&mut self, provider: &str, account: &str, paused: bool) {
        if let Some(state) = self.state_mut(provider, account) {
            state.paused = paused;
        }
    }

    /// Advance and return the counter that rotates new sessions onto different targets.
    pub fn next_rotation(&mut self) -> u64 {
        let value = self.rotation;
        self.rotation += 1;
        value
    }

    /// The target index and reuse count this session settled on for a combo.
    pub fn session_target(&self, session: &str, combo: &str) -> Option<(usize, u16)> {
        let entry = self.sessions.get(session)?;
        let slot = entry.targets.iter().find(|(name, _, _)| name == combo)?;
        Some((slot.1, slot.2))
    }

    /// Remember the combo target a session settled on; `reuses` counts the requests that kept it.
    pub fn set_session_target(&mut self, session: &str, combo: &str, index: usize, reuses: u16) {
        let entry = self.session_or_insert(session);
        match entry.targets.iter_mut().find(|(name, _, _)| name == combo) {
            Some(slot) => {
                slot.1 = index;
                slot.2 = reuses;
            }
            None => entry.targets.push((combo.to_owned(), index, reuses)),
        }
    }

    /// Pick an account for one request: pinned, sticky for the session, else least recently used.
    pub fn pick(
        &mut self,
        provider: &str,
        session: &str,
        pinned: Option<&str>,
        now: Instant,
    ) -> Option<String> {
        let tick = self.bump();
        let bound = self
            .sessions
            .get(session)
            .and_then(|entry| entry.accounts.iter().find(|(name, _)| name == provider))
            .map(|(_, account)| account.clone());
        let states = self.providers.get_mut(provider)?;
        for state in states.iter_mut() {
            if matches!(state.cooldown, Some((until, _)) if until <= now) {
                state.cooldown = None;
            }
        }
        let chosen = match pinned {
            Some(id) => states
                .iter_mut()
                .find(|state| state.id == id && state.usable())
                .map(|state| {
                    state.assigned = tick;
                    state.id.clone()
                }),
            None => {
                let sticky = bound.and_then(|id| {
                    states
                        .iter_mut()
                        .find(|state| state.id == id && state.usable())
                        .map(|state| {
                            state.assigned = tick;
                            state.id.clone()
                        })
                });
                sticky.or_else(|| {
                    let state = states
                        .iter_mut()
                        .filter(|state| state.usable())
                        .min_by_key(|state| state.assigned)?;
                    state.assigned = tick;
                    Some(state.id.clone())
                })
            }
        };
        // A pin is one request's choice; only an automatic pick moves the session.
        if pinned.is_none() {
            if let Some(account) = &chosen {
                self.bind(session, provider, account);
            }
        }
        chosen
    }

    /// Record how an attempt on an account went.
    pub fn report(&mut self, provider: &str, account: &str, outcome: Outcome, now: Instant) {
        let tick = self.bump();
        let cooldown = match outcome {
            Outcome::Success => {
                if let Some(state) = self.state_mut(provider, account) {
                    state.assigned = tick;
                }
                return;
            }
            Outcome::RateLimited { retry_after } => (
                retry_after.unwrap_or(RATE_LIMIT_COOLDOWN),
                Reason::RateLimited,
            ),
            Outcome::Quota { reset_after } => {
                (reset_after.unwrap_or(QUOTA_COOLDOWN), Reason::Quota)
            }
            Outcome::Auth => (AUTH_COOLDOWN, Reason::Auth),
        };
        if let Some(state) = self.state_mut(provider, account) {
            state.cooldown = Some((now + cooldown.0, cooldown.1));
        }
    }

    /// Soonest time an account could serve again, when none is usable now.
    pub fn earliest_free(
        &self,
        provider: &str,
        pinned: Option<&str>,
        now: Instant,
    ) -> Option<Instant> {
        let states = self.providers.get(provider)?;
        let mut soonest: Option<Instant> = None;
        for state in states {
            if pinned.is_some_and(|id| state.id != id) || state.paused {
                continue;
            }
            let until = match state.cooldown {
                Some((until, _)) if until > now => until,
                _ => return None,
            };
            soonest = Some(soonest.map_or(until, |best| best.min(until)));
        }
        soonest
    }

    fn state_mut(&mut self, provider: &str, account: &str) -> Option<&mut AccountState> {
        self.providers
            .get_mut(provider)?
            .iter_mut()
            .find(|state| state.id == account)
    }

    fn bump(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    fn bind(&mut self, session: &str, provider: &str, account: &str) {
        let entry = self.session_or_insert(session);
        match entry.accounts.iter_mut().find(|(name, _)| name == provider) {
            Some(slot) => slot.1 = account.to_owned(),
            None => entry
                .accounts
                .push((provider.to_owned(), account.to_owned())),
        }
    }

    fn session_or_insert(&mut self, session: &str) -> &mut Session {
        if !self.sessions.contains_key(session) {
            if self.sessions.len() >= MAX_SESSIONS {
                self.evict_oldest();
            }
            let seq = self.bump();
            self.sessions.insert(
                session.to_owned(),
                Session {
                    accounts: Vec::new(),
                    targets: Vec::new(),
                    seq,
                },
            );
            self.order.push_back((seq, session.to_owned()));
        }
        self.sessions
            .get_mut(session)
            .expect("session was just inserted")
    }

    fn evict_oldest(&mut self) {
        while let Some((seq, session)) = self.order.pop_front() {
            if self.sessions.get(&session).is_some_and(|e| e.seq == seq) {
                self.sessions.remove(&session);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_of(provider: &str, accounts: &[&str]) -> Pool {
        let mut pool = Pool::new();
        for account in accounts {
            pool.add(provider, account);
        }
        pool
    }

    #[test]
    fn rotates_round_robin_for_new_sessions() {
        let mut pool = pool_of("p", &["a1", "a2", "a3"]);
        let now = Instant::now();
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
        assert_eq!(pool.pick("p", "s2", None, now).as_deref(), Some("a2"));
        assert_eq!(pool.pick("p", "s3", None, now).as_deref(), Some("a3"));
        assert_eq!(pool.pick("p", "s4", None, now).as_deref(), Some("a1"));
    }

    #[test]
    fn session_keeps_its_account() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
        assert_eq!(pool.pick("p", "s2", None, now).as_deref(), Some("a2"));
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
    }

    #[test]
    fn pinning_ignores_stickiness() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        assert_eq!(pool.pick("p", "s1", Some("a2"), now).as_deref(), Some("a2"));
        assert_eq!(pool.pick("p", "s1", Some("nope"), now), None);
        // The pin did not move the session, which still follows its own pick.
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
    }

    #[test]
    fn paused_accounts_are_skipped() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        pool.set_paused("p", "a1", true);
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a2"));
        assert_eq!(pool.pick("p", "s2", Some("a1"), now), None);
        pool.set_paused("p", "a1", false);
        assert_eq!(pool.pick("p", "s2", Some("a1"), now).as_deref(), Some("a1"));
    }

    #[test]
    fn cooldown_moves_the_session_and_expires() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
        pool.report("p", "a1", Outcome::RateLimited { retry_after: None }, now);
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a2"));
        assert_eq!(
            pool.pick("p", "s1", None, now + Duration::from_secs(59))
                .as_deref(),
            Some("a2")
        );
        // Stickiness keeps the session on the replacement; a new session gets the freed account.
        assert_eq!(
            pool.pick("p", "s1", None, now + Duration::from_secs(61))
                .as_deref(),
            Some("a2")
        );
        assert_eq!(
            pool.pick("p", "s2", None, now + Duration::from_secs(61))
                .as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn retry_after_is_honored() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        pool.report(
            "p",
            "a1",
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(5)),
            },
            now,
        );
        assert_eq!(
            pool.pick("p", "s1", None, now + Duration::from_secs(4)),
            None
        );
        assert_eq!(
            pool.earliest_free("p", None, now),
            Some(now + Duration::from_secs(5))
        );
        assert_eq!(
            pool.pick("p", "s1", None, now + Duration::from_secs(5))
                .as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn default_cooldowns_per_reason() {
        let now = Instant::now();
        let mut pool = pool_of("p", &["quota", "auth"]);
        pool.report("p", "quota", Outcome::Quota { reset_after: None }, now);
        pool.report("p", "auth", Outcome::Auth, now);
        assert_eq!(
            pool.earliest_free("p", None, now),
            Some(now + AUTH_COOLDOWN)
        );
        assert_eq!(
            pool.earliest_free("p", Some("quota"), now),
            Some(now + QUOTA_COOLDOWN)
        );
        // A usable account means nothing is waiting to free up.
        assert_eq!(pool.earliest_free("p", Some("missing"), now), None);
    }

    #[test]
    fn success_keeps_the_account_usable() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        pool.report("p", "a1", Outcome::Auth, now - Duration::from_secs(3600));
        pool.report("p", "a1", Outcome::Success, now);
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
    }

    #[test]
    fn session_map_is_bounded() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        for index in 0..MAX_SESSIONS + 10 {
            pool.pick("p", &format!("s{index}"), None, now);
        }
        assert_eq!(pool.sessions.len(), MAX_SESSIONS);
        assert!(pool.session_target("s0", "c").is_none());
        assert!(pool
            .sessions
            .contains_key(&format!("s{}", MAX_SESSIONS + 9)));
    }

    #[test]
    fn combo_targets_are_remembered_per_session() {
        let mut pool = Pool::new();
        assert_eq!(pool.session_target("s1", "c"), None);
        pool.set_session_target("s1", "c", 2, 1);
        assert_eq!(pool.session_target("s1", "c"), Some((2, 1)));
        pool.set_session_target("s1", "c", 3, 2);
        assert_eq!(pool.session_target("s1", "c"), Some((3, 2)));
        assert_eq!(pool.session_target("s1", "other"), None);
    }
}
