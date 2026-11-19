//! The incremental health screen, run in shadow beside the exact evaluation.
//!
//! marginfi's maintenance health is linear in the banks' prices: for each active balance it is
//! shares × share value × biased price × weight ÷ 10^decimals, assets added and liabilities
//! subtracted. A bank's *unit value* (its biased maintenance price times its share value) is the one
//! number that moves, so instead of re-evaluating every holder when a price changes, each account
//! keeps a running estimate of its weighted assets `a` and liabilities `l`, and a price move adds
//! `coef × Δunit` to the holders of that bank: a multiply-add per position, over flat arrays.
//!
//! The screen only filters. An account is *hot* (it needs an exact evaluation) when its estimate,
//! less the rounding error accumulated since it was last anchored to an exact value, is under a
//! small share of its gross value, or when any of its banks cannot be priced. Everything the linear
//! model does not capture (the variable-borrow premium, same-asset e-mode, a stale oracle) either
//! shows up as an error in the shadow comparison or forces the account hot.
//!
//! This module is shadow-only for now: nothing decides on it. See `Liquidator::shadow_*`.

use std::collections::HashMap;

use fixed::types::I80F48;
use marginfi_type_crate::types::{reconcile_emode_configs, RequirementType, RiskTier};
use solana_pubkey::Pubkey;

use crate::{cache::Cache, wrappers::marginfi_account::MarginfiAccountWrapper};

/// The share of an account's gross weighted value (assets plus liabilities) its estimated health
/// may be above zero and still be hot.
pub const DEFAULT_THETA: f64 = 0.001;

/// The rounding error a single `f64` multiply-add can add, relative to the term, with room to
/// spare (the machine epsilon is 2^-52; a multiply and an add each round).
const ROUNDING: f64 = 4.0 * f64::EPSILON;

/// One position of an account in a bank's holder list. A row whose `generation` is not its
/// account's current one belongs to a position the account no longer has.
#[derive(Clone, Copy)]
struct Row {
    account: u32,
    generation: u32,
    /// Shares × weight ÷ 10^decimals.
    coef: f64,
}

#[derive(Default)]
struct BankSlot {
    /// What one share on each side is worth now, `None` when the bank cannot be priced.
    unit_asset: Option<f64>,
    unit_liab: Option<f64>,
    assets: Vec<Row>,
    liabs: Vec<Row>,
}

struct AccountSlot {
    address: Pubkey,
    generation: u32,
    /// Has deposits and liabilities, so it can be liquidated.
    live: bool,
    /// Estimated weighted assets and liabilities (USD).
    a: f64,
    l: f64,
    /// The most the estimate may be off by since it was last anchored.
    err: f64,
    /// Positions on a bank that cannot be priced: the estimate is blind to them.
    unpriced: u32,
}

/// One position to install: the bank, which side, and its coefficient.
pub struct Position {
    pub bank: Pubkey,
    pub liability: bool,
    pub coef: f64,
}

pub struct Screener {
    theta: f64,
    accounts: Vec<AccountSlot>,
    account_index: HashMap<Pubkey, u32>,
    banks: Vec<BankSlot>,
    bank_index: HashMap<Pubkey, u32>,
}

impl Screener {
    pub fn new(theta: f64) -> Self {
        Self {
            theta,
            accounts: Vec::new(),
            account_index: HashMap::new(),
            banks: Vec::new(),
            bank_index: HashMap::new(),
        }
    }

    fn bank_slot(&mut self, bank: &Pubkey) -> u32 {
        if let Some(index) = self.bank_index.get(bank) {
            return *index;
        }
        let index = self.banks.len() as u32;
        self.banks.push(BankSlot::default());
        self.bank_index.insert(*bank, index);
        index
    }

    fn account_slot(&mut self, address: &Pubkey) -> u32 {
        if let Some(index) = self.account_index.get(address) {
            return *index;
        }
        let index = self.accounts.len() as u32;
        self.accounts.push(AccountSlot {
            address: *address,
            generation: 0,
            live: false,
            a: 0.0,
            l: 0.0,
            err: 0.0,
            unpriced: 0,
        });
        self.account_index.insert(*address, index);
        index
    }

    /// Builds every account from the cache, with `unit` giving each bank's unit value on a side
    /// (`true` for liabilities). Call once, after the cache is loaded.
    pub fn build(
        &mut self,
        cache: &Cache,
        unit: impl Fn(&Pubkey, bool) -> Option<f64>,
        accounts: &[Pubkey],
    ) {
        for bank in cache.banks.get_addresses() {
            self.set_unit_values(&bank, unit(&bank, false), unit(&bank, true));
        }
        for address in accounts {
            if let Ok(account) = cache.marginfi_accounts.try_get_account(address) {
                self.rebuild_account(cache, &account);
            }
        }
    }

    /// Re-reads `account`'s positions from the cache: its balances, and the weights its banks'
    /// configuration and e-mode give them. Retires the rows it had before. An account without both
    /// deposits and liabilities is not screened (nobody can liquidate it).
    pub fn rebuild_account(&mut self, cache: &Cache, account: &MarginfiAccountWrapper) {
        let (deposits, liabilities) = account.get_deposits_and_liabilities_shares();
        if deposits.is_empty() || liabilities.is_empty() {
            self.install(&account.address, &[]);
            return;
        }
        let mut banks = HashMap::new();
        for (_, bank) in deposits.iter().chain(liabilities.iter()) {
            match cache.banks.try_get_bank(bank) {
                Ok(wrapper) => {
                    banks.insert(*bank, wrapper);
                }
                // A bank eva doesn't know: nothing can be said about the account.
                Err(_) => {
                    self.install(&account.address, &[]);
                    return;
                }
            }
        }
        // The weights are the program's: collateral at the most favourable of its own and the
        // e-mode weight the liability banks reconcile to.
        let reconciled = reconcile_emode_configs(
            liabilities
                .iter()
                .map(|(_, bank)| banks[bank].bank.emode.emode_config),
            RequirementType::Maintenance,
        );
        let mut positions = Vec::with_capacity(deposits.len() + liabilities.len());
        for (shares, bank) in &deposits {
            let wrapper = &banks[bank];
            // Isolated collateral counts for nothing in health.
            if wrapper.bank.config.risk_tier == RiskTier::Isolated {
                continue;
            }
            let weight = wrapper
                .bank
                .get_asset_weight(RequirementType::Maintenance, &reconciled);
            positions.push(Position {
                bank: *bank,
                liability: false,
                coef: coefficient(*shares, weight, wrapper.bank.get_balance_decimals()),
            });
        }
        for (shares, bank) in &liabilities {
            let wrapper = &banks[bank];
            let weight: I80F48 = wrapper.bank.config.liability_weight_maint.into();
            positions.push(Position {
                bank: *bank,
                liability: true,
                coef: coefficient(*shares, weight, wrapper.bank.get_balance_decimals()),
            });
        }
        self.install(&account.address, &positions);
    }

    /// Replaces `address`'s positions with `positions` (none: the account is no longer screened),
    /// starting its estimate from the current unit values.
    pub fn install(&mut self, address: &Pubkey, positions: &[Position]) {
        let index = self.account_slot(address);
        {
            let slot = &mut self.accounts[index as usize];
            // Every row it had is now stale.
            slot.generation = slot.generation.wrapping_add(1);
            slot.live = !positions.is_empty();
            slot.a = 0.0;
            slot.l = 0.0;
            slot.err = 0.0;
            slot.unpriced = 0;
        }
        let generation = self.accounts[index as usize].generation;
        for position in positions {
            let bank_index = self.bank_slot(&position.bank);
            let bank = &mut self.banks[bank_index as usize];
            let row = Row {
                account: index,
                generation,
                coef: position.coef,
            };
            let (list, unit) = if position.liability {
                (&mut bank.liabs, bank.unit_liab)
            } else {
                (&mut bank.assets, bank.unit_asset)
            };
            list.push(row);
            let slot = &mut self.accounts[index as usize];
            match unit {
                Some(unit) => {
                    if position.liability {
                        slot.l += position.coef * unit;
                    } else {
                        slot.a += position.coef * unit;
                    }
                }
                None => slot.unpriced += 1,
            }
        }
    }

    /// A bank's unit values changed: each holder's estimate moves by its coefficient times the
    /// change. `None` is a bank that cannot be priced now, which makes its holders hot until it can.
    pub fn set_unit_values(&mut self, bank: &Pubkey, asset: Option<f64>, liab: Option<f64>) {
        let bank_index = self.bank_slot(bank) as usize;
        let (old_asset, old_liab) = {
            let slot = &self.banks[bank_index];
            (slot.unit_asset, slot.unit_liab)
        };
        self.banks[bank_index].unit_asset = asset;
        self.banks[bank_index].unit_liab = liab;
        self.apply(bank_index, false, old_asset, asset);
        self.apply(bank_index, true, old_liab, liab);
    }

    fn apply(&mut self, bank_index: usize, liability: bool, old: Option<f64>, new: Option<f64>) {
        if old == new {
            return;
        }
        let accounts = &mut self.accounts;
        let bank = &mut self.banks[bank_index];
        let rows = if liability {
            &mut bank.liabs
        } else {
            &mut bank.assets
        };
        let mut stale = 0usize;
        for row in rows.iter() {
            let slot = &mut accounts[row.account as usize];
            if slot.generation != row.generation {
                stale += 1;
                continue;
            }
            let moved = match (old, new) {
                (Some(old), Some(new)) => {
                    let delta = row.coef * (new - old);
                    slot.err += (row.coef * new).abs().max((row.coef * old).abs()) * ROUNDING;
                    delta
                }
                (None, Some(new)) => {
                    slot.unpriced = slot.unpriced.saturating_sub(1);
                    slot.err += (row.coef * new).abs() * ROUNDING;
                    row.coef * new
                }
                (Some(old), None) => {
                    slot.unpriced += 1;
                    -(row.coef * old)
                }
                (None, None) => 0.0,
            };
            if liability {
                slot.l += moved;
            } else {
                slot.a += moved;
            }
        }
        // Rows of positions accounts have since left are dropped once they are most of the list.
        if stale > rows.len() / 2 {
            rows.retain(|row| accounts[row.account as usize].generation == row.generation);
        }
    }

    /// Replaces an account's estimate with its exact weighted assets and liabilities, clearing the
    /// error it had accumulated.
    pub fn anchor(&mut self, address: &Pubkey, assets: f64, liabilities: f64) {
        if let Some(index) = self.account_index.get(address) {
            let slot = &mut self.accounts[*index as usize];
            slot.a = assets;
            slot.l = liabilities;
            slot.err = 0.0;
        }
    }

    fn hot(&self, slot: &AccountSlot) -> bool {
        slot.live
            && (slot.unpriced > 0 || slot.a - slot.l - slot.err < self.theta * (slot.a + slot.l))
    }

    /// Whether `address` needs an exact evaluation by the screen's reckoning. An account it does
    /// not screen is not hot.
    pub fn is_hot(&self, address: &Pubkey) -> bool {
        self.account_index
            .get(address)
            .is_some_and(|index| self.hot(&self.accounts[*index as usize]))
    }

    /// The estimate for `address`: weighted assets, weighted liabilities and the error bound.
    pub fn estimate(&self, address: &Pubkey) -> Option<(f64, f64, f64)> {
        let slot = &self.accounts[*self.account_index.get(address)? as usize];
        slot.live.then_some((slot.a, slot.l, slot.err))
    }

    /// How many accounts are screened and how many of them are hot.
    pub fn counts(&self) -> (usize, usize) {
        let live = self.accounts.iter().filter(|slot| slot.live).count();
        let hot = self.accounts.iter().filter(|slot| self.hot(slot)).count();
        (live, hot)
    }

    /// The next screened account at or after position `from`, and the position after it, wrapping
    /// round; for a rolling sample.
    pub fn next_live(&self, from: usize) -> Option<(usize, Pubkey)> {
        let len = self.accounts.len();
        (0..len).find_map(|offset| {
            let at = (from + offset) % len;
            self.accounts[at]
                .live
                .then(|| ((at + 1) % len, self.accounts[at].address))
        })
    }
}

/// shares × weight ÷ 10^decimals, as `f64`.
fn coefficient(shares: I80F48, weight: I80F48, decimals: u8) -> f64 {
    shares.to_num::<f64>() * weight.to_num::<f64>() / 10f64.powi(i32::from(decimals))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(bank: Pubkey, liability: bool, coef: f64) -> Position {
        Position {
            bank,
            liability,
            coef,
        }
    }

    /// 10 SOL at $100 and weight 0.8 against $700 of USDC debt: health +$100.
    fn sol_usdc() -> (Screener, Pubkey, Pubkey, Pubkey) {
        let (sol, usdc, account) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut screen = Screener::new(DEFAULT_THETA);
        screen.set_unit_values(&sol, Some(100.0), Some(100.0));
        screen.set_unit_values(&usdc, Some(1.0), Some(1.0));
        screen.install(
            &account,
            &[
                position(sol, false, 10.0 * 0.8),
                position(usdc, true, 700.0),
            ],
        );
        (screen, sol, usdc, account)
    }

    #[test]
    fn a_price_move_changes_the_estimate_by_coefficient_times_the_move() {
        let (mut screen, sol, _, account) = sol_usdc();
        let (a, l, _) = screen.estimate(&account).unwrap();
        assert!((a - 800.0).abs() < 1e-9 && (l - 700.0).abs() < 1e-9);
        assert!(!screen.is_hot(&account), "$100 of health on $1,500 gross");

        // SOL falls 10%: assets 720, health +$20, still 1.3% of gross, not hot.
        screen.set_unit_values(&sol, Some(90.0), Some(90.0));
        let (a, l, _) = screen.estimate(&account).unwrap();
        assert!((a - 720.0).abs() < 1e-9 && (l - 700.0).abs() < 1e-9);
        assert!(!screen.is_hot(&account));

        // Another 2.5% down: assets 702, health +$2, 0.14% of gross: hot is under 0.1%, so not yet.
        screen.set_unit_values(&sol, Some(87.75), Some(87.75));
        assert!(!screen.is_hot(&account));
        // And past zero it is.
        screen.set_unit_values(&sol, Some(80.0), Some(80.0));
        assert!(screen.is_hot(&account), "assets 640 against 700 of debt");
    }

    #[test]
    fn a_rebuilt_account_leaves_no_trace_of_its_old_positions() {
        let (mut screen, sol, usdc, account) = sol_usdc();
        // It repaid everything and withdrew the SOL: no longer screened.
        screen.install(&account, &[]);
        assert!(screen.estimate(&account).is_none());
        // Moves of the banks it used to hold touch nothing now.
        screen.set_unit_values(&sol, Some(1.0), Some(1.0));
        screen.set_unit_values(&usdc, Some(5.0), Some(5.0));
        assert!(!screen.is_hot(&account));

        // It borrows again, half the debt: the estimate starts from the units as they are.
        screen.install(
            &account,
            &[position(sol, false, 8.0), position(usdc, true, 1.0)],
        );
        let (a, l, _) = screen.estimate(&account).unwrap();
        assert!((a - 8.0).abs() < 1e-9 && (l - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_bank_that_cannot_be_priced_makes_its_holders_hot_until_it_can() {
        let (mut screen, sol, _, account) = sol_usdc();
        screen.set_unit_values(&sol, None, None);
        assert!(screen.is_hot(&account), "blind to its collateral");
        screen.set_unit_values(&sol, Some(100.0), Some(100.0));
        assert!(!screen.is_hot(&account));
        let (a, _, _) = screen.estimate(&account).unwrap();
        assert!((a - 800.0).abs() < 1e-9, "back to where it was");
    }

    #[test]
    fn accumulated_error_makes_an_account_hot_sooner_and_anchoring_clears_it() {
        let (mut screen, sol, _, account) = sol_usdc();
        // Health $100 of $1,500 gross, with a pretend error of $100: the lower bound is zero.
        let index = screen.account_index[&account] as usize;
        screen.accounts[index].err = 100.0;
        assert!(screen.is_hot(&account));
        screen.anchor(&account, 800.0, 700.0);
        assert!(!screen.is_hot(&account));
        // Real moves add real, tiny error.
        for step in 0..1_000 {
            let price = 100.0 + f64::from(step % 7) * 0.01;
            screen.set_unit_values(&sol, Some(price), Some(price));
        }
        let (_, _, err) = screen.estimate(&account).unwrap();
        assert!(err > 0.0 && err < 1e-6, "{err}");
    }

    #[test]
    fn stale_rows_are_compacted_and_next_live_walks_the_screened_accounts() {
        let (mut screen, sol, usdc, account) = sol_usdc();
        let other = Pubkey::new_unique();
        screen.install(
            &other,
            &[position(sol, false, 1.0), position(usdc, true, 1.0)],
        );
        for _ in 0..10 {
            screen.install(
                &account,
                &[position(sol, false, 8.0), position(usdc, true, 1.0)],
            );
        }
        screen.set_unit_values(&sol, Some(101.0), Some(101.0));
        let sol_slot = screen.bank_index[&sol] as usize;
        assert!(
            screen.banks[sol_slot].assets.len() <= 4,
            "{} rows left",
            screen.banks[sol_slot].assets.len()
        );

        let (first, address) = screen.next_live(0).unwrap();
        let (_, second) = screen.next_live(first).unwrap();
        assert_ne!(address, second);
        assert_eq!(screen.counts().0, 2);
    }
}
