mod accounts;
mod banks;
pub mod blockhash;
pub mod mints;
mod oracles;
pub mod pyth_prices;
mod tokens;

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use accounts::MarginfiAccountsCache;
use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Result};
use banks::BanksCache;
use fixed::types::I80F48;
use marginfi_type_crate::{
    constants::FEE_STATE_SEED, pdas::derive_kamino_lending_market_authority, types::MarginfiGroup,
};
use mints::MintsCache;
use oracles::OraclesCache;
use solana_clock::Clock;
use solana_pubkey::Pubkey;
use tokens::TokensCache;

use crate::{
    clock_manager,
    juplend_earn::accounts::Lending,
    kamino_lending::accounts::Reserve,
    utils::{accessor, lut_registry::LutRegistry},
    wrappers::{oracle::OracleWrapper, token_account::TokenAccountWrapper},
};

pub struct Cache {
    pub signer_pk: Pubkey,
    pub marginfi_group_address: Pubkey,
    pub marginfi_group: MarginfiGroup,
    pub marginfi_accounts: MarginfiAccountsCache,
    pub banks: BanksCache,
    pub mints: MintsCache,
    pub oracles: OraclesCache,
    pub pyth_prices: pyth_prices::PythPrices,
    /// The newest blockhash, kept fresh in the background.
    pub blockhash: blockhash::RecentBlockhash,
    pub tokens: TokensCache,
    pub clock: Arc<Mutex<Clock>>,
    pub luts: Arc<LutRegistry>,
    pub global_fee_state_key: Pubkey,
    pub global_fee_wallet: Pubkey,
    /// FeeState's `liquidation_max_fee`, re-read in the background: the admin can change it.
    liquidation_max_fee: std::sync::RwLock<I80F48>,
}

#[derive(Clone)]
pub struct KaminoReserve {
    pub address: Pubkey,
    pub reserve: Reserve,
    pub lending_market_authority: Pubkey,
}

impl Cache {
    pub fn new(
        signer_pk: Pubkey,
        marginfi_group_address: Pubkey,
        clock: Arc<Mutex<Clock>>,
        excluded_mints: HashSet<Pubkey>,
    ) -> Self {
        let (global_fee_state_key, _) =
            Pubkey::find_program_address(&[FEE_STATE_SEED.as_bytes()], &marginfi_type_crate::ID);
        Self {
            signer_pk,
            marginfi_group_address,
            marginfi_group: MarginfiGroup::default(),
            marginfi_accounts: MarginfiAccountsCache::default(),
            banks: BanksCache::default(),
            mints: MintsCache::new(excluded_mints),
            oracles: OraclesCache::default(),
            pyth_prices: pyth_prices::PythPrices::default(),
            blockhash: blockhash::RecentBlockhash::default(),
            tokens: TokensCache::default(),
            clock,
            luts: Arc::new(LutRegistry::default()),
            global_fee_state_key,
            global_fee_wallet: Pubkey::default(),
            liquidation_max_fee: std::sync::RwLock::new(I80F48::ZERO),
        }
    }

    /// The premium the program allows a liquidation before any tag: FeeState's
    /// `liquidation_max_fee`, as last read.
    pub fn liquidation_max_fee(&self) -> I80F48 {
        *self
            .liquidation_max_fee
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `clock`, or the shared clock when that is later: for evaluating on oracle accounts read from the
    /// cache a moment ago. The update thread moves the shared clock up to a Scope price's timestamp
    /// before it stores the price, so a clock read after the price always covers it; one read before
    /// (a cycle's snapshot, while updates keep arriving) can be behind it, and marginfi then rejects the
    /// entry as dated in the future.
    pub fn clock_not_before(&self, clock: &Clock) -> Clock {
        match clock_manager::get_clock(&self.clock) {
            Ok(shared) if shared.unix_timestamp > clock.unix_timestamp => shared,
            _ => clock.clone(),
        }
    }

    pub fn set_liquidation_max_fee(&self, fee: I80F48) {
        *self
            .liquidation_max_fee
            .write()
            .unwrap_or_else(|e| e.into_inner()) = fee;
    }

    fn build_kamino_reserve(address: Pubkey, reserve: Reserve) -> KaminoReserve {
        let lending_market_authority =
            derive_kamino_lending_market_authority(&reserve.lending_market).0;
        KaminoReserve {
            address,
            reserve,
            lending_market_authority,
        }
    }

    pub fn try_get_kamino_reserve(&self, address: &Pubkey) -> Result<KaminoReserve> {
        let account = self.oracles.try_get_account(address)?;
        let mut data: &[u8] = &account.data;
        let reserve = Reserve::try_deserialize(&mut data).map_err(|e| {
            anyhow!(
                "Failed to deserialize Kamino reserve {} from OracleCache: {}",
                address,
                e
            )
        })?;

        Ok(Self::build_kamino_reserve(*address, reserve))
    }

    pub fn try_get_juplend_lending_state(&self, address: &Pubkey) -> Result<Lending> {
        let account = self.oracles.try_get_account(address)?;
        let mut data: &[u8] = &account.data;
        Lending::try_deserialize(&mut data).map_err(|e| {
            anyhow!(
                "Failed to deserialize Juplend lending state {} from OracleCache: {}",
                address,
                e
            )
        })
    }

    pub fn try_get_token_wrapper_lenient(
        &self,
        mint_address: &Pubkey,
        token_address: &Pubkey,
    ) -> Result<TokenAccountWrapper<OracleWrapper>> {
        let token_account = self.tokens.try_get_account(token_address)?;
        let bank_address = self.banks.try_get_account_for_mint(mint_address)?;
        let bank_wrapper = self.banks.try_get_bank(&bank_address)?;
        let clock = clock_manager::get_clock(&self.clock)?;
        let oracle_wrapper = OracleWrapper::build_lenient(self, &clock, &bank_address)?;

        Ok(TokenAccountWrapper {
            balance: accessor::amount(&token_account.data)?,
            bank_wrapper,
            oracle_wrapper,
        })
    }

    /// Re-reads the wallet's token accounts from the chain. Geyser updates are applied only when the
    /// liquidator loop drains them, so right after a liquidation lands the cached balances still show
    /// the pre-liquidation state; anything that sizes a swap from them must refresh first.
    /// Re-reads every wallet token account and returns the oldest slot the reads were made at, so
    /// the caller knows how recent the cached balances now are at least. `read` reads a chunk of up
    /// to 100 accounts and returns the slot read at (processed commitment, like the Geyser stream)
    /// and the accounts in order.
    pub fn refresh_wallet_tokens_with(
        &self,
        read: impl Fn(&[Pubkey]) -> Result<(u64, Vec<Option<solana_account::Account>>)>,
    ) -> Result<u64> {
        let addresses = self.mints.get_tokens();
        let mut read_at = u64::MAX;
        for chunk in addresses.chunks(100) {
            let (slot, accounts) = read(chunk)?;
            read_at = read_at.min(slot);
            for (address, account) in chunk.iter().zip(accounts) {
                if let Some(account) = account {
                    self.tokens.try_update_account(*address, account, slot)?;
                }
            }
        }
        Ok(read_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_at(timestamp: i64) -> Cache {
        Cache::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Arc::new(Mutex::new(Clock {
                unix_timestamp: timestamp,
                ..Clock::default()
            })),
            HashSet::new(),
        )
    }

    fn at(timestamp: i64) -> Clock {
        Clock {
            unix_timestamp: timestamp,
            ..Clock::default()
        }
    }

    #[test]
    fn an_evaluation_clock_is_never_behind_the_shared_one() {
        let cache = cache_at(1_000);
        // A snapshot older than the shared clock is brought up to it...
        assert_eq!(cache.clock_not_before(&at(990)).unix_timestamp, 1_000);
        // ...one at or ahead of it is kept.
        assert_eq!(cache.clock_not_before(&at(1_000)).unix_timestamp, 1_000);
        assert_eq!(cache.clock_not_before(&at(1_010)).unix_timestamp, 1_010);
    }

    #[test]
    fn a_price_stored_after_a_cycle_began_is_covered_by_a_clock_read_afterwards() {
        let cache = cache_at(1_000);
        // A background cycle takes its snapshot.
        let snapshot = at(cache.clock.lock().unwrap().unix_timestamp);
        // The update thread then stores a Scope price dated 1_003: it moves the shared clock up to
        // the price's timestamp first, then stores the price.
        crate::clock_manager::advance_to(&cache.clock, 1_003).unwrap();
        let price_timestamp = 1_003;

        // The snapshot alone would call that price dated in the future...
        assert!(price_timestamp > snapshot.unix_timestamp);
        // ...a clock read after the price is stored does not.
        assert!(cache.clock_not_before(&snapshot).unix_timestamp >= price_timestamp);
    }
}
