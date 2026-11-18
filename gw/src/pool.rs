//! Accounts per provider: which one serves a request, and which are cooling down.

use crate::provider::Failure;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Shortest cooldown, so a provider asking for zero cannot spin a retry loop.
const MIN_COOLDOWN: Duration = Duration::from_secs(1);
/// Cooldown for a rate limit that came without `Retry-After`.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
/// Longest a rate-limit or quota cooldown is trusted. Upstream reset hints can be stale or
/// over-long, so an account is probed again at least this often.
const MAX_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// Cooldown after a rejected credential: long enough to stop a retry storm, short enough to recover.
const AUTH_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// How long a provider-disabled account sits out before one request rechecks it.
pub const DISABLED_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a target whose model the provider does not serve sits out before a recheck.
pub const UNAVAILABLE_COOLDOWN: Duration = Duration::from_secs(60 * 60);
/// Sessions untouched for this long are dropped.
const SESSION_IDLE: Duration = Duration::from_secs(60 * 60);
/// Idle sweeps run at most once per interval, keeping the insert path cheap.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Why an account is cooling down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    RateLimited,
    Quota,
    Auth,
    Disabled,
}

/// What one attempt on an account means for that account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// 429; cool it down for `retry_after` or a minute.
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Quota used up; cool it down until `reset_after` or an hour.
    Quota {
        reset_after: Option<Duration>,
    },
    /// Credential rejected; cool it down for five minutes.
    Auth,
    /// Disabled by the provider; sit out six hours.
    Disabled,
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
            Failure::Disabled => Some(Outcome::Disabled),
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
    /// Last time a request used the session; idle sessions are dropped on insert.
    seen: Instant,
}

/// Usable accounts per provider, plus what each session is using.
#[derive(Default)]
pub struct Pool {
    providers: HashMap<String, Vec<AccountState>>,
    sessions: HashMap<String, Session>,
    /// Bumped for every assignment.
    counter: u64,
    /// Bumped for every new session that needs a rotating start (combos use it).
    rotation: u64,
    /// When the session map was last swept for idle entries.
    swept: Option<Instant>,
    /// (provider, model) pairs the provider reported missing, until their recheck.
    unavailable: HashMap<(String, String), Instant>,
}

/// An account as the account commands report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub id: String,
    pub paused: bool,
    /// Cooldown end and reason, when the account is cooling down.
    pub cooldown: Option<(Instant, Reason)>,
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

    /// Sit a model out on one provider until `until`; the first request after it rechecks.
    pub fn set_unavailable(&mut self, provider: &str, model: &str, until: Instant) {
        self.unavailable
            .insert((provider.to_owned(), model.to_owned()), until);
    }

    /// When a model sitting out on a provider may be tried again; `None` when it may now.
    pub fn unavailable_until(&self, provider: &str, model: &str, now: Instant) -> Option<Instant> {
        self.unavailable
            .get(&(provider.to_owned(), model.to_owned()))
            .copied()
            .filter(|until| *until > now)
    }

    /// Pause or resume an account; unknown accounts are ignored.
    pub fn set_paused(&mut self, provider: &str, account: &str, paused: bool) {
        if let Some(state) = self.state_mut(provider, account) {
            state.paused = paused;
        }
    }

    /// Mirror the stored disabled mark: cool the account until `until`, or lift a
    /// disabled cooldown the store no longer has (a manual recheck).
    pub fn set_disabled(&mut self, provider: &str, account: &str, until: Option<Instant>) {
        let Some(state) = self.state_mut(provider, account) else {
            return;
        };
        match until {
            Some(until) => state.cooldown = Some((until, Reason::Disabled)),
            None if matches!(state.cooldown, Some((_, Reason::Disabled))) => state.cooldown = None,
            None => {}
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
    pub fn set_session_target(
        &mut self,
        session: &str,
        combo: &str,
        index: usize,
        reuses: u16,
        now: Instant,
    ) {
        let entry = self.session_or_insert(session, now);
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
                self.bind(session, provider, account, now);
            }
        } else if let Some(entry) = self.sessions.get_mut(session) {
            entry.seen = now;
        }
        chosen
    }

    /// Put a session on an account outside a pick, e.g. one a restart remembered.
    /// False when the account is unknown or cannot serve now.
    pub fn bind_session(
        &mut self,
        provider: &str,
        session: &str,
        account: &str,
        now: Instant,
    ) -> bool {
        let ready = self.providers.get(provider).is_some_and(|states| {
            states.iter().any(|state| {
                state.id == account
                    && !state.paused
                    && state.cooldown.is_none_or(|(until, _)| until <= now)
            })
        });
        if ready {
            self.bind(session, provider, account, now);
        }
        ready
    }

    /// Accounts of a provider with their paused and cooldown state.
    pub fn accounts(&self, provider: &str) -> Vec<AccountInfo> {
        self.providers
            .get(provider)
            .map(|states| {
                states
                    .iter()
                    .map(|state| AccountInfo {
                        id: state.id.clone(),
                        paused: state.paused,
                        cooldown: state.cooldown,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drop an account's cooldown so it is tried again; unknown accounts are ignored.
    pub fn clear_cooldown(&mut self, provider: &str, account: &str) {
        if let Some(state) = self.state_mut(provider, account) {
            state.cooldown = None;
        }
    }

    /// Record how an attempt on an account went; false when the account is unknown.
    pub fn report(
        &mut self,
        provider: &str,
        account: &str,
        outcome: Outcome,
        now: Instant,
    ) -> bool {
        let tick = self.bump();
        let cooldown = match outcome {
            Outcome::Success => {
                let Some(state) = self.state_mut(provider, account) else {
                    return false;
                };
                state.assigned = tick;
                return true;
            }
            Outcome::RateLimited { retry_after } => (
                retry_after.unwrap_or(RATE_LIMIT_COOLDOWN),
                Reason::RateLimited,
            ),
            Outcome::Quota { reset_after } => (reset_after.unwrap_or(MAX_COOLDOWN), Reason::Quota),
            Outcome::Auth => (AUTH_COOLDOWN, Reason::Auth),
            Outcome::Disabled => (DISABLED_COOLDOWN, Reason::Disabled),
        };
        let Some(state) = self.state_mut(provider, account) else {
            return false;
        };
        // A provider may send `Retry-After: 0`; a zero cooldown would retry forever.
        let cap = match cooldown.1 {
            Reason::RateLimited | Reason::Quota => MAX_COOLDOWN,
            _ => Duration::MAX,
        };
        state.cooldown = Some((now + cooldown.0.clamp(MIN_COOLDOWN, cap), cooldown.1));
        true
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

    fn bind(&mut self, session: &str, provider: &str, account: &str, now: Instant) {
        let entry = self.session_or_insert(session, now);
        match entry.accounts.iter_mut().find(|(name, _)| name == provider) {
            Some(slot) => slot.1 = account.to_owned(),
            None => entry
                .accounts
                .push((provider.to_owned(), account.to_owned())),
        }
    }

    fn session_or_insert(&mut self, session: &str, now: Instant) -> &mut Session {
        if !self.sessions.contains_key(session) {
            let due = self.swept.is_none_or(|last| now >= last + SWEEP_EVERY);
            if due {
                self.sessions
                    .retain(|_, entry| now.saturating_duration_since(entry.seen) <= SESSION_IDLE);
                self.swept = Some(now);
            }
            self.sessions.insert(
                session.to_owned(),
                Session {
                    accounts: Vec::new(),
                    targets: Vec::new(),
                    seen: now,
                },
            );
        }
        let entry = self
            .sessions
            .get_mut(session)
            .expect("session was just inserted");
        entry.seen = now;
        entry
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
            Some(now + MAX_COOLDOWN)
        );
        // A usable account means nothing is waiting to free up.
        assert_eq!(pool.earliest_free("p", Some("missing"), now), None);
    }

    #[test]
    fn long_reset_hints_are_capped_so_the_account_is_probed_again() {
        let now = Instant::now();
        let mut pool = pool_of("p", &["a1", "a2"]);
        let hours = Some(Duration::from_secs(3 * 3600));
        pool.report("p", "a1", Outcome::Quota { reset_after: hours }, now);
        pool.report("p", "a2", Outcome::RateLimited { retry_after: hours }, now);
        assert_eq!(
            pool.earliest_free("p", Some("a1"), now),
            Some(now + MAX_COOLDOWN)
        );
        assert_eq!(
            pool.earliest_free("p", Some("a2"), now),
            Some(now + MAX_COOLDOWN)
        );
    }

    #[test]
    fn disabled_accounts_sit_out_until_rechecked() {
        let now = Instant::now();
        let mut pool = pool_of("p", &["a1"]);
        pool.report("p", "a1", Outcome::Disabled, now);
        assert_eq!(
            pool.earliest_free("p", None, now),
            Some(now + DISABLED_COOLDOWN)
        );
        // The store dropping the mark (a manual recheck) frees it at once.
        pool.set_disabled("p", "a1", None);
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
        // Other cooldowns are not the store's to lift.
        pool.report("p", "a1", Outcome::Auth, now);
        pool.set_disabled("p", "a1", None);
        assert_eq!(pool.pick("p", "s2", None, now), None);
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
    fn session_map_drops_idle_sessions() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        pool.pick("p", "old", None, now);
        pool.pick("p", "fresh", None, now + Duration::from_secs(10));
        // The sweep runs on the next insert, once the interval has passed.
        let later = now + SESSION_IDLE + SWEEP_EVERY;
        pool.pick("p", "new", None, later);
        assert!(!pool.sessions.contains_key("old"));
        assert!(!pool.sessions.contains_key("fresh"));
        assert!(pool.sessions.contains_key("new"));
    }

    #[test]
    fn combo_targets_are_remembered_per_session() {
        let now = Instant::now();
        let mut pool = Pool::new();
        assert_eq!(pool.session_target("s1", "c"), None);
        pool.set_session_target("s1", "c", 2, 1, now);
        assert_eq!(pool.session_target("s1", "c"), Some((2, 1)));
        pool.set_session_target("s1", "c", 3, 2, now);
        assert_eq!(pool.session_target("s1", "c"), Some((3, 2)));
        assert_eq!(pool.session_target("s1", "other"), None);
    }

    #[test]
    fn report_reports_whether_it_recorded_anything() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        assert!(pool.report("p", "a1", Outcome::Auth, now));
        assert!(pool.report("p", "a1", Outcome::Success, now));
        assert!(!pool.report("p", "missing", Outcome::Auth, now));
        assert!(!pool.report("nope", "a1", Outcome::Success, now));
    }

    #[test]
    fn accounts_are_listed_with_their_state() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        pool.set_paused("p", "a2", true);
        pool.report("p", "a1", Outcome::Auth, now);
        let listed = pool.accounts("p");
        assert_eq!(
            listed,
            [
                AccountInfo {
                    id: "a1".into(),
                    paused: false,
                    cooldown: Some((now + AUTH_COOLDOWN, Reason::Auth)),
                },
                AccountInfo {
                    id: "a2".into(),
                    paused: true,
                    cooldown: None,
                },
            ]
        );
        assert!(pool.accounts("missing").is_empty());
    }

    #[test]
    fn clearing_a_cooldown_makes_the_account_usable() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        pool.report("p", "a1", Outcome::Auth, now);
        assert_eq!(pool.pick("p", "s1", None, now), None);
        pool.clear_cooldown("p", "a1");
        assert!(pool.accounts("p")[0].cooldown.is_none());
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a1"));
    }

    #[test]
    fn failure_maps_to_account_state() {
        let wait = Duration::from_secs(7);
        let fatal = Failure::Fatal {
            status: http::StatusCode::BAD_REQUEST,
            body: bytes::Bytes::new(),
        };
        let cases = [
            (
                Failure::RateLimited {
                    retry_after: Some(wait),
                },
                Some(Outcome::RateLimited {
                    retry_after: Some(wait),
                }),
            ),
            (
                Failure::RateLimited { retry_after: None },
                Some(Outcome::RateLimited { retry_after: None }),
            ),
            (
                Failure::Quota {
                    reset_after: Some(wait),
                },
                Some(Outcome::Quota {
                    reset_after: Some(wait),
                }),
            ),
            (
                Failure::Quota { reset_after: None },
                Some(Outcome::Quota { reset_after: None }),
            ),
            (Failure::Auth, Some(Outcome::Auth)),
            (Failure::Disabled, Some(Outcome::Disabled)),
            (Failure::Retryable, None),
            (fatal, None),
        ];
        for (failure, expected) in cases {
            assert_eq!(Outcome::from_failure(&failure), expected, "{failure:?}");
        }
    }

    #[test]
    fn zero_retry_after_still_cools_down() {
        let mut pool = pool_of("p", &["a1"]);
        let now = Instant::now();
        pool.report(
            "p",
            "a1",
            Outcome::RateLimited {
                retry_after: Some(Duration::ZERO),
            },
            now,
        );
        assert_eq!(pool.pick("p", "s1", None, now), None);
        assert_eq!(pool.earliest_free("p", None, now), Some(now + MIN_COOLDOWN));
        assert_eq!(
            pool.pick("p", "s1", None, now + MIN_COOLDOWN).as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn pinned_account_that_is_cooling_is_not_picked() {
        let mut pool = pool_of("p", &["a1", "a2"]);
        let now = Instant::now();
        pool.report(
            "p",
            "a1",
            Outcome::Quota {
                reset_after: Some(Duration::from_secs(120)),
            },
            now,
        );
        assert_eq!(pool.pick("p", "s1", Some("a1"), now), None);
        assert_eq!(
            pool.earliest_free("p", Some("a1"), now),
            Some(now + Duration::from_secs(120))
        );
        // The other account is free, so an unpinned pick works and nothing is pending.
        assert_eq!(pool.pick("p", "s1", None, now).as_deref(), Some("a2"));
        assert_eq!(pool.earliest_free("p", None, now), None);
    }
}
