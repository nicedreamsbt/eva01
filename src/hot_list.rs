//! The hot list: the accounts worth checking the moment a price or an account changes.
//!
//! Evaluating every borrower on every update is far too slow, but only the accounts close to the
//! liquidation line can flip on a price move, and only those that could pay `min_profit` are worth
//! a liquidation. The background evaluation (the health watch, the evaluation cycles and the sweep)
//! tells the list about every account it evaluates; the update path re-evaluates only the list's
//! members that a change touches, with the same exact evaluation, and queues what it finds.
//!
//! An account is on the list while its margin is under a cutoff and the most a liquidation of it
//! could earn is at least `min_profit`. The list is capped; when it is full the account closest to
//! the line stays.

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use solana_pubkey::Pubkey;

use crate::health_watch::Watch;

/// An account is hot when its margin, (assets - liabilities) / (assets + liabilities) on weighted
/// values, is under this: the share of its gross value a move against it can take before it can be
/// liquidated, in the worst case where every position moves against it at once.
pub const DEFAULT_MARGIN_CUTOFF: f64 = 0.04;

/// The most accounts the list holds.
pub const DEFAULT_MAX_ACCOUNTS: usize = 2_000;

/// Threads the hot path evaluates on by default. Few: it checks a short list, and the box it runs on
/// is shared with the background evaluation, which the operating system keeps these ahead of.
pub const DEFAULT_THREADS: usize = 2;

/// What the update path does with the list (`HOT_PATH`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotMode {
    /// The list is not kept and nothing is checked on updates: the background path alone.
    Off,
    /// The update path evaluates hot accounts and logs what it would have queued, but queues
    /// nothing; the background path decides, and the log shows how much earlier the hot path was.
    Shadow,
    /// The update path evaluates hot accounts and queues what it finds.
    On,
}

#[derive(Clone, Copy, Debug)]
pub struct HotConfig {
    pub mode: HotMode,
    pub margin_cutoff: f64,
    pub max_accounts: usize,
    /// Threads the hot path evaluates on (`HOT_THREADS`), a pool of its own.
    pub threads: usize,
}

impl HotConfig {
    pub fn from_env() -> Self {
        let mode = match std::env::var("HOT_PATH").as_deref().map(str::trim) {
            Ok("off") => HotMode::Off,
            Ok("shadow") => HotMode::Shadow,
            Ok("on") | Ok("") | Err(_) => HotMode::On,
            Ok(other) => {
                log::warn!("HOT_PATH={other:?} is not off, shadow or on; using on");
                HotMode::On
            }
        };
        let number = |var: &str| std::env::var(var).ok().filter(|v| !v.trim().is_empty());
        let margin_cutoff = number("HOT_MARGIN_CUTOFF")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(DEFAULT_MARGIN_CUTOFF);
        let max_accounts = number("HOT_MAX_ACCOUNTS")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_ACCOUNTS);
        let threads = number("HOT_THREADS")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_THREADS);
        Self {
            mode,
            margin_cutoff,
            max_accounts,
            threads,
        }
    }
}

/// What the last exact evaluation of an account found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Standing {
    /// (weighted assets - weighted liabilities) / (weighted assets + weighted liabilities);
    /// negative for an account that is unhealthy.
    pub margin: f64,
    /// The most a liquidation of the account could earn (USD): an upper bound, so an account it
    /// rules out can never pay `min_profit`.
    pub potential_profit: f64,
}

struct Entry {
    margin: f64,
    banks: Vec<Pubkey>,
    /// What the account was last evaluated at, to tell when a price move could matter to it; `None`
    /// means every move of its banks does.
    watch: Option<Watch>,
    /// When the account joined the list.
    joined: Instant,
    /// When the update path last evaluated it.
    checked: Instant,
}

pub struct HotList {
    margin_cutoff: f64,
    max_accounts: usize,
    min_profit: f64,
    entries: HashMap<Pubkey, Entry>,
    /// The hot accounts holding each bank, so a price move finds them without looking at anyone
    /// else.
    by_bank: HashMap<Pubkey, HashSet<Pubkey>>,
    /// Accounts that qualified and found the list full of accounts closer to the line.
    overflowed: usize,
}

impl HotList {
    pub fn new(config: &HotConfig, min_profit: f64) -> Self {
        Self {
            margin_cutoff: config.margin_cutoff,
            max_accounts: config.max_accounts,
            min_profit,
            entries: HashMap::new(),
            by_bank: HashMap::new(),
            overflowed: 0,
        }
    }

    fn qualifies(&self, standing: &Standing) -> bool {
        standing.margin < self.margin_cutoff && standing.potential_profit >= self.min_profit
    }

    /// Records what an evaluation found out about `account`, which holds `banks`: it joins the list,
    /// stays on it or leaves it. `checked_at` is set by the update path when it did the evaluating.
    pub fn observe(
        &mut self,
        account: Pubkey,
        standing: Standing,
        banks: Vec<Pubkey>,
        watch: Option<Watch>,
        now: Instant,
        checked_at: Option<Instant>,
    ) {
        if !self.qualifies(&standing) {
            self.remove(&account);
            return;
        }
        if !self.entries.contains_key(&account) && self.entries.len() >= self.max_accounts {
            // Full: the account closest to the line stays.
            let worst = self
                .entries
                .iter()
                .max_by(|a, b| a.1.margin.total_cmp(&b.1.margin))
                .map(|(address, entry)| (*address, entry.margin));
            match worst {
                Some((worst, margin)) if standing.margin < margin => self.remove(&worst),
                _ => {
                    self.overflowed += 1;
                    return;
                }
            }
        }
        let (joined, checked) = match self.entries.get(&account) {
            Some(entry) => (entry.joined, entry.checked),
            None => (now, now),
        };
        // Re-index the banks if the account's holdings changed.
        if let Some(entry) = self.entries.get(&account) {
            if entry.banks != banks {
                let old = entry.banks.clone();
                self.unindex(&account, &old);
                self.index(account, &banks);
            }
        } else {
            self.index(account, &banks);
        }
        self.entries.insert(
            account,
            Entry {
                margin: standing.margin,
                banks,
                watch,
                joined,
                checked: checked_at.unwrap_or(checked),
            },
        );
    }

    fn index(&mut self, account: Pubkey, banks: &[Pubkey]) {
        for bank in banks {
            self.by_bank.entry(*bank).or_default().insert(account);
        }
    }

    fn unindex(&mut self, account: &Pubkey, banks: &[Pubkey]) {
        for bank in banks {
            if let Some(holders) = self.by_bank.get_mut(bank) {
                holders.remove(account);
                if holders.is_empty() {
                    self.by_bank.remove(bank);
                }
            }
        }
    }

    pub fn remove(&mut self, account: &Pubkey) {
        if let Some(entry) = self.entries.remove(account) {
            self.unindex(account, &entry.banks);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn contains(&self, account: &Pubkey) -> bool {
        self.entries.contains_key(account)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The hot accounts that hold any of `banks`.
    pub fn holders_of<'a>(&self, banks: impl IntoIterator<Item = &'a Pubkey>) -> HashSet<Pubkey> {
        banks
            .into_iter()
            .filter_map(|bank| self.by_bank.get(bank))
            .flatten()
            .copied()
            .collect()
    }

    /// The hot holders of the banks that moved that the move could matter to: those whose watch says
    /// a position's unit value (`current`, for the banks that could be priced) has moved far enough
    /// since they were last evaluated, and every holder of a bank in `unpriced`, which cannot be
    /// told. A bank's share value moves on every deposit or borrow, which changes no account's
    /// health by anything that matters.
    pub fn needing_evaluation<'a>(
        &self,
        moved: impl IntoIterator<Item = &'a Pubkey>,
        current: &HashMap<(Pubkey, bool), f64>,
        unpriced: impl IntoIterator<Item = &'a Pubkey>,
    ) -> HashSet<Pubkey> {
        let mut needing: HashSet<Pubkey> = moved
            .into_iter()
            .filter_map(|bank| self.by_bank.get(bank))
            .flatten()
            .filter(|account| {
                self.entries
                    .get(*account)
                    .is_some_and(|entry| entry.watch.as_ref().is_none_or(|w| w.moved(current)))
            })
            .copied()
            .collect();
        needing.extend(self.holders_of(unpriced));
        needing
    }

    /// The hot accounts that are in `accounts`: a change that touches thousands of accounts (a bank
    /// reconfigured) costs a walk over the list, not over them.
    pub fn among(&self, accounts: &HashSet<Pubkey>) -> Vec<Pubkey> {
        if accounts.len() < self.entries.len() {
            accounts
                .iter()
                .filter(|account| self.entries.contains_key(*account))
                .copied()
                .collect()
        } else {
            self.entries
                .keys()
                .filter(|account| accounts.contains(*account))
                .copied()
                .collect()
        }
    }

    /// Whether `account` was on the list at `since`.
    pub fn was_hot_at(&self, account: &Pubkey, since: Instant) -> bool {
        self.entries
            .get(account)
            .is_some_and(|entry| entry.joined <= since)
    }

    /// Whether the update path evaluated `account` after `since`: any result an older evaluation
    /// produced for it is out of date.
    pub fn checked_since(&self, account: &Pubkey, since: Instant) -> bool {
        self.entries
            .get(account)
            .is_some_and(|entry| entry.checked > since)
    }

    /// How many accounts qualified but did not fit, since the last call.
    pub fn take_overflowed(&mut self) -> usize {
        std::mem::take(&mut self.overflowed)
    }

    /// The smallest, median and largest margin on the list, for the minute report.
    pub fn margin_range(&self) -> Option<(f64, f64, f64)> {
        let mut margins: Vec<f64> = self.entries.values().map(|entry| entry.margin).collect();
        if margins.is_empty() {
            return None;
        }
        margins.sort_by(f64::total_cmp);
        Some((
            margins[0],
            margins[margins.len() / 2],
            margins[margins.len() - 1],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(max_accounts: usize) -> HotList {
        HotList::new(
            &HotConfig {
                mode: HotMode::On,
                margin_cutoff: 0.04,
                max_accounts,
                threads: 1,
            },
            1.0,
        )
    }

    fn standing(margin: f64, potential_profit: f64) -> Standing {
        Standing {
            margin,
            potential_profit,
        }
    }

    fn observe(list: &mut HotList, account: Pubkey, standing: Standing, banks: &[Pubkey]) {
        list.observe(
            account,
            standing,
            banks.to_vec(),
            None,
            Instant::now(),
            None,
        );
    }

    #[test]
    fn an_account_is_hot_while_it_is_near_the_line_and_could_pay() {
        let mut list = list(10);
        let (near, far, dust, unhealthy, bank) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        observe(&mut list, near, standing(0.03, 5.0), &[bank]);
        observe(&mut list, far, standing(0.10, 50.0), &[bank]);
        observe(&mut list, dust, standing(0.01, 0.2), &[bank]);
        observe(&mut list, unhealthy, standing(-0.02, 3.0), &[bank]);

        assert!(list.contains(&near));
        assert!(!list.contains(&far), "too far from the line");
        assert!(!list.contains(&dust), "can never pay the minimum");
        assert!(list.contains(&unhealthy), "already past the line");

        // A move in the wrong direction takes it off the list again.
        observe(&mut list, near, standing(0.09, 5.0), &[bank]);
        assert!(!list.contains(&near));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn a_price_move_finds_only_the_hot_holders_of_that_bank() {
        let mut list = list(10);
        let (sol, usdc, eth) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        observe(&mut list, a, standing(0.01, 5.0), &[sol, usdc]);
        observe(&mut list, b, standing(0.02, 5.0), &[eth, usdc]);
        observe(&mut list, c, standing(0.50, 5.0), &[sol]);

        assert_eq!(list.holders_of(&[sol]), HashSet::from([a]));
        assert_eq!(list.holders_of(&[usdc]), HashSet::from([a, b]));
        assert_eq!(list.holders_of(&[sol, eth]), HashSet::from([a, b]));

        // The account swaps its collateral: the index follows it.
        observe(&mut list, a, standing(0.01, 5.0), &[eth, usdc]);
        assert!(list.holders_of(&[sol]).is_empty());
        assert_eq!(list.holders_of(&[eth]), HashSet::from([a, b]));

        list.remove(&a);
        assert_eq!(list.holders_of(&[usdc]), HashSet::from([b]));
    }

    #[test]
    fn a_full_list_keeps_the_accounts_closest_to_the_line() {
        let mut list = list(2);
        let bank = Pubkey::new_unique();
        let (a, b, c, d) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        observe(&mut list, a, standing(0.03, 5.0), &[bank]);
        observe(&mut list, b, standing(0.02, 5.0), &[bank]);

        // Further from the line than both: turned away.
        observe(&mut list, c, standing(0.035, 5.0), &[bank]);
        assert!(!list.contains(&c));
        assert_eq!(list.take_overflowed(), 1);

        // Closer than the furthest: it takes that one's place.
        observe(&mut list, d, standing(0.01, 5.0), &[bank]);
        assert!(list.contains(&d) && list.contains(&b));
        assert!(!list.contains(&a));
        assert_eq!(list.len(), 2);
        assert_eq!(list.holders_of(&[bank]), HashSet::from([b, d]));
    }

    #[test]
    fn results_older_than_an_update_path_check_can_be_told_apart() {
        let mut list = list(10);
        let (account, bank) = (Pubkey::new_unique(), Pubkey::new_unique());
        let started = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));

        // Joined (by the background path) after `started`: it was not hot then.
        list.observe(
            account,
            standing(0.01, 5.0),
            vec![bank],
            None,
            Instant::now(),
            None,
        );
        assert!(!list.was_hot_at(&account, started));
        assert!(list.was_hot_at(&account, Instant::now()));
        assert!(!list.checked_since(&account, Instant::now()));

        // The update path checks it: a background cycle that began before is out of date for it.
        let cycle_start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        list.observe(
            account,
            standing(0.01, 5.0),
            vec![bank],
            None,
            Instant::now(),
            Some(Instant::now()),
        );
        assert!(list.checked_since(&account, cycle_start));

        // A background observation does not make it look freshly checked.
        let later = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        list.observe(
            account,
            standing(0.01, 5.0),
            vec![bank],
            None,
            Instant::now(),
            None,
        );
        assert!(!list.checked_since(&account, later));
    }

    #[test]
    fn only_the_hot_holders_a_move_could_matter_to_are_evaluated() {
        use crate::health_watch::WatchedPosition;
        let mut list = list(10);
        let (sol, usdc, other) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let position = |bank, liability, unit_value| WatchedPosition {
            bank,
            liability,
            unit_value,
        };
        let (tight, loose, unwatched, elsewhere) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let on = |list: &mut HotList, account, tolerance: Option<f64>, banks: &[Pubkey]| {
            let watch = tolerance.map(|tolerance| {
                Watch::with_tolerance(
                    tolerance,
                    vec![position(sol, false, 100.0), position(usdc, true, 1.0)],
                )
            });
            list.observe(
                account,
                standing(0.01, 5.0),
                banks.to_vec(),
                watch,
                Instant::now(),
                None,
            );
        };
        on(&mut list, tight, Some(0.001), &[sol, usdc]);
        on(&mut list, loose, Some(0.05), &[sol, usdc]);
        on(&mut list, unwatched, None, &[sol, usdc]);
        on(&mut list, elsewhere, Some(0.001), &[other]);

        // SOL moves 0.5%: past the tight watch, inside the loose one; the account with no watch is
        // always evaluated; the one that does not hold SOL is not looked at.
        let current = HashMap::from([((sol, false), 100.5), ((sol, true), 100.5)]);
        let needing = list.needing_evaluation([&sol], &current, std::iter::empty());
        assert_eq!(needing, HashSet::from([tight, unwatched]));

        // USDC share value drifting by a rounding moves no one.
        let drift = HashMap::from([((usdc, true), 1.000_000_01), ((usdc, false), 1.000_000_01)]);
        let needing = list.needing_evaluation([&usdc], &drift, std::iter::empty());
        assert_eq!(needing, HashSet::from([unwatched]));

        // A bank that could not be priced: every hot holder, whatever its watch.
        let none = HashMap::new();
        let needing = list.needing_evaluation(std::iter::empty(), &none, [&usdc]);
        assert_eq!(needing, HashSet::from([tight, loose, unwatched]));
    }

    #[test]
    fn the_mode_and_cutoffs_are_read_from_the_environment() {
        // No variable set in the test environment: the defaults.
        let config = HotConfig::from_env();
        assert_eq!(config.mode, HotMode::On);
        assert_eq!(config.margin_cutoff, DEFAULT_MARGIN_CUTOFF);
        assert_eq!(config.max_accounts, DEFAULT_MAX_ACCOUNTS);
    }
}
