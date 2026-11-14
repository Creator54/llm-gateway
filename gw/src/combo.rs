//! Combos: try targets in order and move on when one fails before the client got a byte.

use crate::pool::{Outcome, Pool};
use std::time::Instant;

/// How a combo orders the targets it tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Listed order, falling back to the next target until one replies.
    Failover,
    /// Weighted order whose starting point rotates for every new session.
    RoundRobin,
}

/// One provider model a combo can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Provider id, as the account pool keys it.
    pub provider: String,
    /// Provider's own model id.
    pub model: String,
    /// Share of new sessions that start here; round-robin only.
    pub weight: u32,
}

/// An ordered set of targets and how to pick between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    /// Name used by `combo/<name>`.
    pub name: String,
    /// How targets are ordered.
    pub strategy: Strategy,
    /// Targets in listed order; each weight defaults to 1 when zero.
    pub targets: Vec<Target>,
    /// Requests a session keeps a round-robin target before rotating to the next one.
    pub sticky_limit: u16,
}

impl Combo {
    /// Attempts for one request, in the order they may be tried.
    // Failover is only safe before the first reply byte: stop iterating once a target replies,
    // or the prompt is sent upstream twice.
    pub fn attempt<'a>(
        &'a self,
        pool: &'a mut Pool,
        session: &'a str,
        now: Instant,
    ) -> Attempt<'a> {
        let start = self.start(pool, session);
        let count = self.targets.len();
        let order = (0..count).map(|step| (start + step) % count).collect();
        Attempt {
            combo: self,
            pool,
            session,
            now,
            order,
            pos: 0,
            again: None,
            last: None,
        }
    }

    /// Where this request starts: the remembered target, else the head or a rotated one.
    fn start(&self, pool: &mut Pool, session: &str) -> usize {
        let count = self.targets.len();
        if count == 0 {
            return 0;
        }
        let kept = pool
            .session_target(session, &self.name)
            .filter(|(index, _)| *index < count);
        match (kept, self.strategy) {
            (Some((index, _)), Strategy::Failover) => index,
            (Some((index, reuses)), Strategy::RoundRobin) => {
                if reuses >= self.sticky_limit.max(1) {
                    (index + 1) % count
                } else {
                    index
                }
            }
            (None, Strategy::Failover) => 0,
            (None, Strategy::RoundRobin) => weighted_start(&self.targets, pool.next_rotation()),
        }
    }
}

/// Targets one request may still try, plus the attempt last handed out.
pub struct Attempt<'a> {
    combo: &'a Combo,
    pool: &'a mut Pool,
    session: &'a str,
    now: Instant,
    order: Vec<usize>,
    pos: usize,
    /// Target whose account just cooled down and may still have another account free.
    again: Option<usize>,
    last: Option<(usize, String)>,
}

impl<'a> Iterator for Attempt<'a> {
    type Item = (&'a Target, String);

    fn next(&mut self) -> Option<Self::Item> {
        // An account move stays inside the same target before the target itself is given up.
        if let Some(index) = self.again.take() {
            if let Some(item) = self.try_target(index) {
                return Some(item);
            }
        }
        while let Some(&index) = self.order.get(self.pos) {
            self.pos += 1;
            if let Some(item) = self.try_target(index) {
                return Some(item);
            }
        }
        self.last = None;
        None
    }
}

impl<'a> Attempt<'a> {
    /// A target with no usable account is skipped; `retry_after` reports why.
    fn try_target(&mut self, index: usize) -> Option<(&'a Target, String)> {
        let target = &self.combo.targets[index];
        let account = self
            .pool
            .pick(&target.provider, self.session, None, self.now)?;
        self.last = Some((index, account.clone()));
        Some((target, account))
    }

    /// Record how the attempt just handed out went.
    pub fn report(&mut self, outcome: Outcome) {
        let Some((index, account)) = self.last.take() else {
            return;
        };
        let provider = self.combo.targets[index].provider.as_str();
        self.pool.report(provider, &account, outcome, self.now);
        if outcome != Outcome::Success {
            // The cooled account cannot be picked again at this `now`, so this terminates.
            self.again = Some(index);
        }
    }

    /// Record that the attempt just handed out is replying; the session stays on this target.
    pub fn note_success(&mut self) {
        let Some((index, account)) = &self.last else {
            return;
        };
        let provider = self.combo.targets[*index].provider.as_str();
        let index = *index;
        self.pool
            .report(provider, account, Outcome::Success, self.now);
        let reuses = match self.pool.session_target(self.session, &self.combo.name) {
            Some((kept, reuses)) if kept == index => reuses.saturating_add(1),
            _ => 1,
        };
        self.pool
            .set_session_target(self.session, &self.combo.name, index, reuses);
    }

    /// Soonest time a skipped target could serve again; `None` when no cooldown will end.
    pub fn retry_after(&self) -> Option<Instant> {
        self.combo
            .targets
            .iter()
            .filter_map(|target| self.pool.earliest_free(&target.provider, None, self.now))
            .min()
    }
}

/// First target of the weighted ring at `rotation`, so new sessions spread by weight.
fn weighted_start(targets: &[Target], rotation: u64) -> usize {
    let with_weights = || targets.iter().map(|target| u64::from(target.weight.max(1)));
    let total: u64 = with_weights().sum();
    let mut slot = rotation % total.max(1);
    for (index, weight) in with_weights().enumerate() {
        if slot < weight {
            return index;
        }
        slot -= weight;
    }
    targets.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::Outcome;
    use std::time::Duration;

    fn target(provider: &str) -> Target {
        Target {
            provider: provider.to_owned(),
            model: "m".to_owned(),
            weight: 1,
        }
    }

    fn combo(strategy: Strategy, providers: &[&str]) -> Combo {
        Combo {
            name: "c".to_owned(),
            strategy,
            targets: providers.iter().map(|p| target(p)).collect(),
            sticky_limit: 1,
        }
    }

    fn pool(providers: &[(&str, &str)]) -> Pool {
        let mut pool = Pool::new();
        for (provider, account) in providers {
            pool.add(provider, account);
        }
        pool
    }

    #[test]
    fn failover_uses_listed_order() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1"), ("c", "c1")]);
        let combo = combo(Strategy::Failover, &["a", "b", "c"]);
        let now = Instant::now();
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        let models: Vec<&str> = attempts
            .by_ref()
            .map(|(t, _)| t.provider.as_str())
            .collect();
        assert_eq!(models, ["a", "b", "c"]);
    }

    #[test]
    fn skips_targets_without_usable_accounts() {
        let mut pool = pool(&[("a", "a1"), ("c", "c1")]);
        let combo = combo(Strategy::Failover, &["a", "b", "c"]);
        let now = Instant::now();
        pool.set_paused("a", "a1", true);
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        assert_eq!(attempts.next().unwrap().0.provider, "c");
        assert!(attempts.next().is_none());
    }

    #[test]
    fn a_cooled_account_moves_the_attempt_to_the_next_account() {
        let mut pool = pool(&[("a", "a1"), ("a", "a2")]);
        let combo = combo(Strategy::Failover, &["a"]);
        let now = Instant::now();
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        let (_, first) = attempts.next().unwrap();
        assert_eq!(first, "a1");
        attempts.report(Outcome::RateLimited { retry_after: None });
        let (_, second) = attempts.next().unwrap();
        assert_eq!(second, "a2");
        assert!(attempts.next().is_none());
    }

    #[test]
    fn a_session_sticks_to_the_target_that_worked() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1")]);
        let combo = combo(Strategy::Failover, &["a", "b"]);
        let now = Instant::now();
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        let (_, account) = attempts.next().unwrap();
        attempts.report(Outcome::Auth);
        assert_eq!(account, "a1");
        let (target, _) = attempts.next().unwrap();
        assert_eq!(target.provider, "b");
        attempts.note_success();
        assert_eq!(pool.session_target("s1", "c"), Some((1, 1)));

        // Provider "a" recovered, but the session stays on the fallback until it fails.
        let mut later = combo.attempt(&mut pool, "s1", now);
        assert_eq!(later.next().unwrap().0.provider, "b");

        // Once "a" recovered, a failing fallback goes back to the listed order.
        let later = now + Duration::from_secs(360);
        let mut attempts = combo.attempt(&mut pool, "s1", later);
        assert_eq!(attempts.next().unwrap().0.provider, "b");
        attempts.report(Outcome::RateLimited { retry_after: None });
        assert_eq!(attempts.next().unwrap().0.provider, "a");
        attempts.note_success();
        assert_eq!(pool.session_target("s1", "c"), Some((0, 1)));
    }

    #[test]
    fn round_robin_rotates_the_start_per_session() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1"), ("c", "c1")]);
        let combo = combo(Strategy::RoundRobin, &["a", "b", "c"]);
        let now = Instant::now();
        let starts: Vec<String> = ["s1", "s2", "s3"]
            .iter()
            .map(|session| {
                combo
                    .attempt(&mut pool, session, now)
                    .next()
                    .unwrap()
                    .0
                    .provider
                    .clone()
            })
            .collect();
        assert_eq!(starts, ["a", "b", "c"]);
    }

    #[test]
    fn round_robin_weights_the_starting_point() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1")]);
        let mut combo = combo(Strategy::RoundRobin, &["a", "b"]);
        combo.targets[0].weight = 3;
        let now = Instant::now();
        let starts: Vec<String> = ["s1", "s2", "s3", "s4"]
            .iter()
            .map(|session| {
                combo
                    .attempt(&mut pool, session, now)
                    .next()
                    .unwrap()
                    .0
                    .provider
                    .clone()
            })
            .collect();
        assert_eq!(starts, ["a", "a", "a", "b"]);
    }

    #[test]
    fn round_robin_keeps_a_target_until_the_sticky_limit() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1")]);
        let mut combo = combo(Strategy::RoundRobin, &["a", "b"]);
        combo.sticky_limit = 2;
        let now = Instant::now();
        for _ in 0..2 {
            let mut attempts = combo.attempt(&mut pool, "s1", now);
            assert_eq!(attempts.next().unwrap().0.provider, "a");
            attempts.note_success();
        }
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        assert_eq!(attempts.next().unwrap().0.provider, "b");
    }

    #[test]
    fn all_targets_cooling_reports_the_soonest_retry() {
        let mut pool = pool(&[("a", "a1"), ("b", "b1")]);
        let combo = combo(Strategy::Failover, &["a", "b"]);
        let now = Instant::now();
        pool.report(
            "a",
            "a1",
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(90)),
            },
            now,
        );
        pool.report(
            "b",
            "b1",
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            now,
        );
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        assert!(attempts.next().is_none());
        assert_eq!(attempts.retry_after(), Some(now + Duration::from_secs(30)));
    }

    #[test]
    fn an_empty_combo_yields_nothing() {
        let mut pool = Pool::new();
        let combo = combo(Strategy::Failover, &[]);
        let now = Instant::now();
        let mut attempts = combo.attempt(&mut pool, "s1", now);
        assert!(attempts.next().is_none());
        assert_eq!(attempts.retry_after(), None);
    }
}
