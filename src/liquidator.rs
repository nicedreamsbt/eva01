use crate::{
    cache::Cache,
    config::Eva01Config,
    metrics::{ERROR_COUNT, FAILED_LIQUIDATIONS, LIQUIDATION_LATENCY},
    rebalancer::Rebalancer,
    thread_debug, thread_error, thread_info, thread_trace, thread_warn,
    utils::{calc_total_weighted_assets_liabs, get_free_collateral, swb_cranker::SwbCranker},
    wrappers::{
        liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use marginfi::state::{
    bank::BankImpl,
    marginfi_account::RequirementType,
    price::{OraclePriceType, PriceAdapter, PriceBias},
};
use marginfi_type_crate::{
    constants::{BANKRUPT_THRESHOLD, EXP_10_I80F48},
    types::{BalanceSide, BankOperationalState, RiskTier},
};
use solana_program::pubkey::Pubkey;
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use std::{sync::atomic::Ordering, thread};

#[cfg(feature = "publish_to_db")]
use crate::utils::supabase::{PublishQueue, SupabasePublisher, Threshold};

const DECLARED_VALUE_RANGE: f64 = 0.5;

pub struct Liquidator {
    liquidator_account: Arc<LiquidatorAccount>,
    rebalancer: Rebalancer,
    support_isolated_banks: bool,
    min_profit: f64,
    run_liquidation: Arc<AtomicBool>,
    stop_liquidator: Arc<AtomicBool>,
    cache: Arc<Cache>,
    swb_cranker: SwbCranker,
    declared_values: HashMap<Pubkey, f64>,
    #[cfg(feature = "publish_to_db")]
    pub publish_thresholds: Vec<Threshold>,
}

pub struct PreparedLiquidatableAccount {
    liquidatee_account: MarginfiAccountWrapper,
    asset_bank: Pubkey,
    liab_bank: Pubkey,
    asset_amount: u64,
    liab_amount: u64,
    profit: u64,
}

impl Liquidator {
    pub fn new(
        config: Eva01Config,
        liquidator_account: Arc<LiquidatorAccount>,
        run_liquidation: Arc<AtomicBool>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let swb_cranker = SwbCranker::new(&config.general_config)?;

        let rebalancer =
            Rebalancer::new(config.clone(), liquidator_account.clone(), cache.clone())?;

        Ok(Liquidator {
            liquidator_account,
            rebalancer,
            support_isolated_banks: config.liquidator_config.isolated_banks,
            min_profit: config.general_config.min_profit,
            run_liquidation,
            stop_liquidator,
            cache,
            swb_cranker,
            declared_values: config.general_config.declared_values,
            #[cfg(feature = "publish_to_db")]
            publish_thresholds: config.general_config.publish_thresholds,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        // Fund the liquidator account, if needed
        if !self.liquidator_account.has_funds()? {
            return Err(anyhow!("Liquidator has no funds."));
        }

        self.rebalancer.run()?;

        #[cfg(feature = "publish_to_db")]
        let mut publish_queue =
            PublishQueue::new(std::mem::take(&mut self.publish_thresholds), Instant::now());

        #[cfg(feature = "publish_to_db")]
        let mut next_publishing = publish_queue.pop().unwrap();
        #[cfg(feature = "publish_to_db")]
        let mut supabase = SupabasePublisher::from_env()?;

        thread_info!("Staring the Liquidator loop.");
        let mut last_evaluation = Instant::now();
        let evaluation_interval = Duration::from_secs(30); // Run evaluations every 30 seconds
        
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            let should_run_evaluation = self.run_liquidation.load(Ordering::Relaxed) 
                || last_evaluation.elapsed() > evaluation_interval;
                
            if should_run_evaluation {
                thread_info!("Running the Liquidation process...");
                self.run_liquidation.store(false, Ordering::Relaxed);
                last_evaluation = Instant::now(); // Update timestamp for periodic evaluations

                if let Ok(mut accounts) = self.evaluate_all_accounts() {
                    // Accounts are sorted from the highest profit to the lowest
                    accounts.sort_by(|a, b| a.profit.cmp(&b.profit));
                    accounts.reverse();

                    let mut stale_swb_oracles: HashSet<Pubkey> = HashSet::new();
                    for candidate in accounts {
                        match self.process_account(&candidate.liquidatee_account) {
                            Ok(acc_opt) => {
                                if let Some(acc) = acc_opt {
                                    let start = Instant::now();
                                    if let Err(e) = &self.liquidator_account.liquidate(
                                        &acc.liquidatee_account,
                                        &acc.asset_bank,
                                        &acc.liab_bank,
                                        acc.asset_amount,
                                        acc.liab_amount,
                                        &stale_swb_oracles,
                                    ) {
                                        thread_error!(
                                            "Failed to liquidate account {:?}, error: {:?}",
                                            candidate.liquidatee_account.address,
                                            e.error
                                        );
                                        FAILED_LIQUIDATIONS.inc();
                                        ERROR_COUNT.inc();
                                        stale_swb_oracles.extend(&e.keys);
                                    }
                                    let duration = start.elapsed().as_secs_f64();
                                    LIQUIDATION_LATENCY.observe(duration);
                                }
                            }
                            Err(e) => {
                                thread_error!(
                                    "The account {:?} has failed the liquidation evaluation: {:?}",
                                    candidate.liquidatee_account.address,
                                    e
                                );
                                ERROR_COUNT.inc();
                            }
                        }
                    }
                    if !stale_swb_oracles.is_empty() {
                        thread_info!("Cranking Swb Oracles {:#?}", stale_swb_oracles);
                        if let Err(err) = self
                            .swb_cranker
                            .crank_oracles(stale_swb_oracles.into_iter().collect())
                        {
                            thread_error!("Failed to crank Swb Oracles: {}", err)
                        }
                        thread_info!("Completed cranking Swb Oracles.");
                    };
                }

                thread_info!("The Liquidation process is complete.");

                if let Err(error) = self.rebalancer.run() {
                    thread_error!("Rebalancing failed: {:?}", error);
                    ERROR_COUNT.inc();
                }
            } else {
                thread::sleep(Duration::from_secs(1))
            }

            // TODO: gather info and crank any stale swb oracles
            #[cfg(feature = "publish_to_db")]
            if Instant::now() > next_publishing.next {
                if let Err(e) = self.publish_all_accounts_health(
                    &mut supabase,
                    next_publishing.rule.min_liab_value_usd,
                ) {
                    thread_error!("Failed to publish all accounts' health: {}", e);
                }
                next_publishing = publish_queue.rotate_next(next_publishing);
            }
        }
        thread_info!("The Liquidator loop is stopped.");

        Ok(())
    }

    /// Check a specific account for liquidation opportunities
    pub fn evaluate_specific_account(&mut self, account: &MarginfiAccountWrapper) -> Result<Option<PreparedLiquidatableAccount>> {
        if account.address == self.liquidator_account.liquidator_address {
            return Ok(None);
        }

        // Quick health check first
        match self.quick_health_check(account) {
            Ok(health) => {
                if health < 0.0 {
                    // Account is underwater, process it
                    self.process_account(account)
                } else {
                    Ok(None)
                }
            }
            Err(_) => Ok(None)
        }
    }

    /// Check accounts that use a specific oracle
    pub fn evaluate_accounts_using_oracle(&mut self, oracle_address: &Pubkey) -> Result<Vec<PreparedLiquidatableAccount>> {
        let mut result: Vec<PreparedLiquidatableAccount> = vec![];
        
        thread_info!("Finding accounts using oracle {}", oracle_address);
        
        // Find banks that use this oracle
        let banks_using_oracle: Vec<Pubkey> = self.cache.banks
            .iter()
            .filter(|(_, bank_wrapper)| {
                // Check if this bank uses the oracle
                bank_wrapper.bank.config.oracle_keys.contains(oracle_address)
            })
            .map(|(bank_address, _)| *bank_address)
            .collect();
        
        if banks_using_oracle.is_empty() {
            thread_info!("No banks found using oracle {}", oracle_address);
            return Ok(result);
        }
        
        thread_info!("Found {} banks using oracle {}, checking accounts with positions in these banks", 
                    banks_using_oracle.len(), oracle_address);
        
        // Check all accounts for positions in these banks
        let total_accounts = self.cache.marginfi_accounts.len()?;
        let mut processed = 0;
        
        for index in 0..total_accounts {
            match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                Ok(account) => {
                    if account.address == self.liquidator_account.liquidator_address {
                        continue;
                    }
                    
                    // Check if this account has positions in any of the banks using this oracle
                    let (deposit_shares, liab_shares) = account.get_deposits_and_liabilities_shares();
                    
                    let has_relevant_positions = deposit_shares.iter().any(|(_, bank_pk)| banks_using_oracle.contains(bank_pk))
                        || liab_shares.iter().any(|(_, bank_pk)| banks_using_oracle.contains(bank_pk));
                    
                    if has_relevant_positions {
                        // Quick health check first
                        match self.quick_health_check(&account) {
                            Ok(health) => {
                                if health < 0.0 {
                                    // Account is underwater, process it
                                    match self.process_account(&account) {
                                        Ok(acc_opt) => {
                                            if let Some(acc) = acc_opt {
                                                result.push(acc);
                                            }
                                        }
                                        Err(e) => {
                                            thread_trace!("Failed to process account {:?}: {:?}", account.address, e);
                                            ERROR_COUNT.inc();
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                // If health check fails, skip this account
                            }
                        }
                    }
                    
                    processed += 1;
                }
                Err(err) => {
                    thread_error!("Failed to get Marginfi account by index {}: {:?}", index, err);
                    ERROR_COUNT.inc();
                }
            }
        }
        
        thread_info!("Oracle-based evaluation complete: processed {} accounts, found {} liquidatable accounts using oracle {}", 
                    processed, result.len(), oracle_address);
        
        Ok(result)
    }

    /// Check accounts that have positions in a specific bank
    pub fn evaluate_accounts_with_positions_in_bank(&mut self, bank_address: &Pubkey) -> Result<Vec<PreparedLiquidatableAccount>> {
        let mut result: Vec<PreparedLiquidatableAccount> = vec![];
        
        thread_info!("Finding accounts with positions in bank {}", bank_address);
        
        // Check all accounts for positions in this specific bank
        let total_accounts = self.cache.marginfi_accounts.len()?;
        let mut processed = 0;
        let mut accounts_with_positions = 0;
        
        for index in 0..total_accounts {
            match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                Ok(account) => {
                    if account.address == self.liquidator_account.liquidator_address {
                        continue;
                    }
                    
                    // Check if this account has positions in this specific bank
                    let (deposit_shares, liab_shares) = account.get_deposits_and_liabilities_shares();
                    
                    let has_position_in_bank = deposit_shares.iter().any(|(_, bank_pk)| bank_pk == bank_address)
                        || liab_shares.iter().any(|(_, bank_pk)| bank_pk == bank_address);
                    
                    if has_position_in_bank {
                        accounts_with_positions += 1;
                        
                        // Quick health check first
                        match self.quick_health_check(&account) {
                            Ok(health) => {
                                if health < 0.0 {
                                    // Account is underwater, process it
                                    match self.process_account(&account) {
                                        Ok(acc_opt) => {
                                            if let Some(acc) = acc_opt {
                                                result.push(acc);
                                            }
                                        }
                                        Err(e) => {
                                            thread_trace!("Failed to process account {:?}: {:?}", account.address, e);
                                            ERROR_COUNT.inc();
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                // If health check fails, skip this account
                            }
                        }
                    }
                    
                    processed += 1;
                }
                Err(err) => {
                    thread_error!("Failed to get Marginfi account by index {}: {:?}", index, err);
                    ERROR_COUNT.inc();
                }
            }
        }
        
        thread_info!("Bank-based evaluation complete: processed {} accounts, found {} accounts with positions in bank {}, {} liquidatable", 
                    processed, accounts_with_positions, bank_address, result.len());
        
        Ok(result)
    }

    /// Execute liquidation for a prepared liquidatable account
    pub fn execute_liquidation(&mut self, prepared_account: &PreparedLiquidatableAccount) -> Result<()> {
        let start = std::time::Instant::now();
        
        thread_info!("Executing liquidation for account {} - profit: ${}", 
                    prepared_account.liquidatee_account.address, 
                    prepared_account.profit as f64 / 1_000_000.0);
        
        let mut stale_swb_oracles: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
        
        match self.liquidator_account.liquidate(
            &prepared_account.liquidatee_account,
            &prepared_account.asset_bank,
            &prepared_account.liab_bank,
            prepared_account.asset_amount,
            prepared_account.liab_amount,
            &stale_swb_oracles,
        ) {
            Ok(_) => {
                let duration = start.elapsed().as_secs_f64();
                crate::metrics::LIQUIDATION_LATENCY.observe(duration);
                thread_info!("Successfully liquidated account {} in {:.2}s", 
                            prepared_account.liquidatee_account.address, duration);
            }
            Err(e) => {
                thread_error!("Failed to liquidate account {:?}: {:?}", 
                             prepared_account.liquidatee_account.address, e.error);
                crate::metrics::FAILED_LIQUIDATIONS.inc();
                crate::metrics::ERROR_COUNT.inc();
                stale_swb_oracles.extend(&e.keys);
            }
        }
        
        // Crank any stale SWB oracles
        if !stale_swb_oracles.is_empty() {
            thread_info!("Cranking SWB oracles for failed liquidation: {:#?}", stale_swb_oracles);
            if let Err(err) = self.swb_cranker.crank_oracles(stale_swb_oracles.into_iter().collect()) {
                thread_error!("Failed to crank SWB oracles: {}", err);
            }
        }
        
        Ok(())
    }

    /// Execute multiple liquidations in sequence
    pub fn execute_liquidations(&mut self, prepared_accounts: Vec<PreparedLiquidatableAccount>) -> Result<()> {
        if prepared_accounts.is_empty() {
            return Ok(());
        }
        
        thread_info!("Executing {} liquidations", prepared_accounts.len());
        
        // Sort by profit (highest first)
        let mut sorted_accounts = prepared_accounts;
        sorted_accounts.sort_by(|a, b| b.profit.cmp(&a.profit));
        
        let mut successful = 0;
        let mut failed = 0;
        
        for prepared_account in sorted_accounts {
            match self.execute_liquidation(&prepared_account) {
                Ok(_) => successful += 1,
                Err(e) => {
                    failed += 1;
                    thread_error!("Failed to execute liquidation for account {}: {:?}", 
                                 prepared_account.liquidatee_account.address, e);
                }
            }
        }
        
        thread_info!("Liquidation execution complete: {} successful, {} failed", successful, failed);
        
        // Run rebalancer after liquidations
        if let Err(error) = self.rebalancer.run() {
            thread_error!("Rebalancing failed after liquidations: {:?}", error);
            crate::metrics::ERROR_COUNT.inc();
        }
        
        Ok(())
    }

    /// Checks if liquidation is needed, for each account one by one
    /// Optimized to only process accounts that are likely to be underwater
    fn evaluate_all_accounts(&mut self) -> Result<Vec<PreparedLiquidatableAccount>> {
        let mut result: Vec<PreparedLiquidatableAccount> = vec![];
        let total_accounts = self.cache.marginfi_accounts.len()?;
        let mut processed = 0;
        let max_accounts_to_process = total_accounts; // Process all accounts - reactive system handles performance
        
        thread_info!("Starting liquidation evaluation of {} accounts (limited to {} for performance)", total_accounts, max_accounts_to_process);
        
        let mut index: usize = 0;
        while index < total_accounts && processed < max_accounts_to_process {
            match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                Ok(account) => {
                    if account.address == self.liquidator_account.liquidator_address {
                        index += 1;
                        continue;
                    }
                    
                    // Quick health check first to avoid expensive processing of healthy accounts
                    match self.quick_health_check(&account) {
                        Ok(health) => {
                            if health < 0.0 {
                                // Only process underwater accounts
                                match self.process_account(&account) {
                                    Ok(acc_opt) => {
                                        if let Some(acc) = acc_opt {
                                            result.push(acc);
                                        }
                                    }
                                    Err(e) => {
                                        thread_trace!(
                                            "Failed to process account {:?}: {:?}",
                                            account.address,
                                            e
                                        );
                                        ERROR_COUNT.inc();
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            // If health check fails, skip this account
                        }
                    }
                    processed += 1;
                }
                Err(err) => {
                    thread_error!(
                        "Failed to get Marginfi account by index {}: {:?}",
                        index,
                        err
                    );
                    ERROR_COUNT.inc();
                }
            }
            index += 1;
        }
        
        thread_info!("Completed liquidation evaluation: processed {} accounts, found {} liquidatable accounts", processed, result.len());
        Ok(result)
    }
    
    /// Quick health check to avoid processing healthy accounts
    fn quick_health_check(&self, account: &MarginfiAccountWrapper) -> Result<f64> {
        let (deposit_shares, liabs_shares) = account.get_deposits_and_liabilities_shares();
        
        if liabs_shares.is_empty() {
            return Ok(100.0); // No liabilities = healthy
        }
        
        let deposit_values = self.get_value_of_shares(
            deposit_shares,
            &BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;
        
        let liab_values = self.get_value_of_shares(
            liabs_shares,
            &BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;
        
        let total_weighted_assets: I80F48 = deposit_values.iter().map(|(v, _)| *v).sum();
        let total_weighted_liabilities: I80F48 = liab_values.iter().map(|(v, _)| *v).sum();
        
        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        
        if total_weighted_assets == I80F48::ZERO {
            if total_weighted_liabilities == I80F48::ZERO {
                return Ok(100.0);
            } else {
                return Ok(0.0);
            }
        } else {
            Ok(maintenance_health.checked_div(total_weighted_assets).unwrap_or(I80F48::ZERO).to_num::<f64>() * 100.0)
        }
    }

    #[cfg(feature = "publish_to_db")]
    fn publish_all_accounts_health(
        &mut self,
        supabase: &mut SupabasePublisher,
        min_liab_value_usd: f64,
    ) -> Result<()> {
        let mut index: usize = 0;
        let total_accs = self.cache.marginfi_accounts.len()?;
        while index < total_accs {
            match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                Ok(account) => {
                    if let Err(e) =
                        self.publish_account_health(&account, supabase, min_liab_value_usd)
                    {
                        thread_error!(
                            "Failed to publish Marginfi account health {}: {:?}",
                            account.address,
                            e
                        );
                    }
                }
                Err(err) => {
                    thread_error!(
                        "Failed to get Marginfi account by index {}: {:?}",
                        index,
                        err
                    );
                    ERROR_COUNT.inc();
                }
            }
            index += 1;
        }
        supabase.flush_all()?;

        Ok(())
    }

    #[cfg(feature = "publish_to_db")]
    fn publish_account_health(
        &self,
        account: &MarginfiAccountWrapper,
        supabase: &mut SupabasePublisher,
        min_liab_value_usd: f64,
    ) -> Result<()> {
        let (total_weighted_assets, total_weighted_liabilities) = calc_total_weighted_assets_liabs(
            &self.cache,
            &account.lending_account,
            RequirementType::Maintenance,
        )?;
        if total_weighted_liabilities < min_liab_value_usd {
            return Ok(());
        }

        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        let percentage_health = if total_weighted_assets == I80F48::ZERO {
            if total_weighted_liabilities == I80F48::ZERO {
                100.0
            } else {
                0.0
            }
        } else {
            maintenance_health
                .checked_div(total_weighted_assets)
                .unwrap()
                .to_num::<f64>()
                * 100.0
        };

        supabase.publish_health(
            account.address,
            total_weighted_assets.to_num::<f64>(),
            total_weighted_liabilities.to_num::<f64>(),
            maintenance_health.to_num::<f64>(),
            percentage_health,
        )?;

        Ok(())
    }

    fn process_account(
        &self,
        account: &MarginfiAccountWrapper,
    ) -> Result<Option<PreparedLiquidatableAccount>> {
        let (deposit_shares, liabs_shares) = account.get_deposits_and_liabilities_shares();
        if liabs_shares.is_empty() {
            return Ok(None);
        }

        let deposit_values = self.get_value_of_shares(
            deposit_shares,
            &BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_values = self.get_value_of_shares(
            liabs_shares,
            &BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let (asset_bank_pk, liab_bank_pk) =
            match self.find_liquidation_bank_candidates(deposit_values, liab_values)? {
                Some(banks) => banks,
                None => return Ok(None),
            };

        // Calculated max liquidatable amount is the defining factor for liquidation.
        let (max_liquidatable_asset_amount, max_liquidatable_liab_amount, profit) = self
            .compute_max_liquidatable_amounts_with_banks(account, &asset_bank_pk, &liab_bank_pk)?;

        if max_liquidatable_asset_amount.is_zero() {
            return Ok(None);
        }

        let (max_liab_coverage_amount, max_liab_coverage_value) =
            self.get_max_liquidation_capacity_with_usdc(&liab_bank_pk)?;

        // Asset
        let asset_bank_wrapper = self.cache.banks.try_get_bank(&asset_bank_pk)?;
        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, &asset_bank_pk)?;

        let liquidation_asset_amount_capacity = asset_bank_wrapper.calc_amount(
            &asset_oracle_wrapper,
            max_liab_coverage_value,
            BalanceSide::Assets,
            RequirementType::Initial,
        )?;

        let asset_amount_to_liquidate = min(
            max_liquidatable_asset_amount,
            liquidation_asset_amount_capacity,
        );
        let liab_amount_to_liquidate = min(max_liquidatable_liab_amount, max_liab_coverage_amount);

        let slippage_adjusted_asset_amount = asset_amount_to_liquidate * I80F48!(0.90);
        let slippage_adjusted_liab_amount = liab_amount_to_liquidate * I80F48!(0.90);

        thread_debug!(
                "Liquidation asset amount capacity: {:?}, asset_amount_to_liquidate: {:?}, slippage_adjusted_asset_amount: {:?}",
                liquidation_asset_amount_capacity, asset_amount_to_liquidate, slippage_adjusted_asset_amount
            );

        Ok(Some(PreparedLiquidatableAccount {
            liquidatee_account: account.clone(),
            asset_bank: asset_bank_pk,
            liab_bank: liab_bank_pk,
            asset_amount: slippage_adjusted_asset_amount.to_num(),
            liab_amount: slippage_adjusted_liab_amount.to_num(),
            profit: profit.to_num(),
        }))
    }

    // TODO: simplify this
    fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> Result<(I80F48, I80F48)> {
        let lq_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;

        let free_collateral = get_free_collateral(&self.cache, lq_account)?;

        let bank = self.cache.banks.try_get_bank(bank_pk)?;
        let oracle_wrapper = OracleWrapper::build(&self.cache, bank_pk)?;

        let (asset_amount, _) = self.get_balance_for_bank(lq_account, bank_pk)?;
        thread_debug!(
            "Liquidator Asset amount: {:?}, free collateral: {:?}",
            asset_amount,
            free_collateral
        );

        let untied_collateral_for_bank = min(
            free_collateral,
            bank.calc_value(
                &oracle_wrapper,
                asset_amount,
                BalanceSide::Assets,
                RequirementType::Initial,
            )?,
        );
        thread_debug!(
            "Liquidator Untied collateral for bank: {:?}",
            untied_collateral_for_bank
        );

        let asset_weight: I80F48 = bank.bank.config.asset_weight_init.into();
        let liab_weight: I80F48 = bank.bank.config.asset_weight_init.into();
        let oracle_max_confidence = bank.bank.config.oracle_max_confidence;

        let lower_price = oracle_wrapper.get_price_of_type(
            OraclePriceType::TimeWeighted,
            Some(PriceBias::Low),
            oracle_max_confidence,
        )?;

        let higher_price = oracle_wrapper.get_price_of_type(
            OraclePriceType::TimeWeighted,
            Some(PriceBias::High),
            oracle_max_confidence,
        )?;

        let token_decimals = bank.bank.mint_decimals as usize;

        let max_borrow_amount = if asset_weight == I80F48::ZERO {
            let max_additional_borrow_ui =
                (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            let max_additional = max_additional_borrow_ui * EXP_10_I80F48[token_decimals];

            max_additional + asset_amount
        } else {
            let ui_amount = untied_collateral_for_bank / (lower_price * asset_weight)
                + (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            ui_amount * EXP_10_I80F48[token_decimals]
        };
        let max_borrow_value = bank.calc_value(
            &oracle_wrapper,
            max_borrow_amount,
            BalanceSide::Liabilities,
            RequirementType::Initial,
        )?;

        thread_debug!(
            "Liquidator asset_weight: {:?}, max borrow amount: {:?}, max borrow value: {:?}",
            asset_weight,
            max_borrow_amount,
            max_borrow_value
        );

        Ok((max_borrow_amount, max_borrow_value))
    }

    /// Calculate maximum liquidation capacity using USDC collateral
    /// This allows cross-asset liquidations by using the liquidator's USDC to pay liabilities
    fn get_max_liquidation_capacity_with_usdc(&self, liab_bank_pk: &Pubkey) -> Result<(I80F48, I80F48)> {
        let lq_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;

        let free_collateral = get_free_collateral(&self.cache, lq_account)?;
        
        // Get the liability bank to determine the token price
        let liab_bank = self.cache.banks.try_get_bank(liab_bank_pk)?;
        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank_pk)?;
        
        // Use conservative pricing for liquidation capacity calculation
        let liab_price = liab_oracle_wrapper.get_price_of_type(
            marginfi::state::price::OraclePriceType::TimeWeighted,
            Some(marginfi::state::price::PriceBias::High), // Use high price for conservative estimate
            liab_bank.bank.config.oracle_max_confidence,
        )?;
        
        let token_decimals = liab_bank.bank.mint_decimals as usize;
        
        // Calculate how much of the liability token we can buy with our USDC collateral
        // Apply a safety factor to account for slippage and price volatility
        let safety_factor = I80F48::from_num(0.85); // 15% safety margin
        let max_liab_value_usdc = free_collateral * safety_factor;
        
        // Convert USDC value to liability token amount
        let max_liab_amount_ui = max_liab_value_usdc / liab_price;
        let max_liab_amount = max_liab_amount_ui * I80F48::from_num(10_u64.pow(token_decimals as u32));
        
        // Calculate the value in USD for capacity comparison
        let max_liab_value = liab_bank.calc_value(
            &liab_oracle_wrapper,
            max_liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Initial,
        )?;
        
        thread_debug!(
            "Liquidation capacity with USDC: free_collateral={:?}, liab_price={:?}, max_liab_amount={:?}, max_liab_value={:?}",
            free_collateral, liab_price, max_liab_amount, max_liab_value
        );
        
        Ok((max_liab_amount, max_liab_value))
    }

    fn find_liquidation_bank_candidates(
        &self,
        deposit_values: Vec<(I80F48, Pubkey)>,
        liab_values: Vec<(I80F48, Pubkey)>,
    ) -> Result<Option<(Pubkey, Pubkey)>> {
        if deposit_values.is_empty() || liab_values.is_empty() {
            return Ok(None);
        }

        if deposit_values
            .iter()
            .map(|(v, _)| v.to_num::<f64>())
            .sum::<f64>()
            < BANKRUPT_THRESHOLD
        {
            return Ok(None);
        }

        let (_, asset_bank) = deposit_values
            .iter()
            .max_by(|a, b| {
                //debug!("Asset Bank {:?} value: {:?}", a.1, a.0);
                a.0.cmp(&b.0)
            })
            .ok_or_else(|| anyhow!("No asset bank found"))?;

        let (_, liab_bank) = liab_values
            .iter()
            .max_by(|a, b| {
                //debug!("Liab Bank {:?} value: {:?}", a.1, a.0);

                a.0.cmp(&b.0)
            })
            .ok_or_else(|| anyhow!("No liability bank found"))?;

        Ok(Some((*asset_bank, *liab_bank)))
    }

    fn compute_max_liquidatable_amounts_with_banks(
        &self,
        account: &MarginfiAccountWrapper,
        asset_bank_pk: &Pubkey,
        liab_bank_pk: &Pubkey,
    ) -> Result<(I80F48, I80F48, I80F48)> {
        let (total_weighted_assets, total_weighted_liabilities) = calc_total_weighted_assets_liabs(
            &self.cache,
            &account.lending_account,
            RequirementType::Maintenance,
        )?;
        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        thread_trace!(
            "Account {} maintenance_health = {:?}",
            account.address,
            maintenance_health
        );
        if maintenance_health >= I80F48::ZERO {
            // TODO: revisit this crazy return type
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let asset_bank_wrapper = self.cache.banks.try_get_bank(asset_bank_pk)?;
        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, asset_bank_pk)?;
          let asset_price = asset_oracle_wrapper
            .get_price_of_type(OraclePriceType::RealTime, None, 0)?
            .to_num::<f64>();
        if let Some(&declared_value) = self.declared_values.get(&asset_bank_wrapper.bank.mint) {
            let min_asset_price = declared_value * (1.0 - DECLARED_VALUE_RANGE);
            if asset_price < min_asset_price {
                thread_warn!(
                    "Asset ({}) price is lower than the declared range: {} < {}",
                    asset_bank_wrapper.bank.mint,
                    asset_price,
                    min_asset_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
            let max_asset_price = declared_value * (1.0 + DECLARED_VALUE_RANGE);
            if asset_price > max_asset_price {
                thread_warn!(
                    "Asset ({}) price is higher than the declared range: {} > {}",
                    asset_bank_wrapper.bank.mint,
                    asset_price,
                    max_asset_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
        }
        let liab_bank_wrapper = self.cache.banks.try_get_bank(liab_bank_pk)?;
        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank_pk)?;
          let liab_price = liab_oracle_wrapper
            .get_price_of_type(OraclePriceType::RealTime, None, 0)?
            .to_num::<f64>();
        if let Some(&declared_value) = self.declared_values.get(&liab_bank_wrapper.bank.mint) {
            let min_liab_price = declared_value * (1.0 - DECLARED_VALUE_RANGE);
            if liab_price < min_liab_price {
                thread_warn!(
                    "Liability ({}) price is lower than the declared range: {} < {}",
                    liab_bank_wrapper.bank.mint,
                    liab_price,
                    min_liab_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
            let max_liab_price = declared_value * (1.0 + DECLARED_VALUE_RANGE);
            if liab_price > max_liab_price {
                thread_warn!(
                    "Liability ({}) price is higher than the declared range: {} > {}",
                    liab_bank_wrapper.bank.mint,
                    liab_price,
                    max_liab_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
        }

        let asset_weight_maint: I80F48 = asset_bank_wrapper.bank.config.asset_weight_maint.into();
        let liab_weight_maint: I80F48 = liab_bank_wrapper.bank.config.liability_weight_maint.into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let all = asset_weight_maint - liab_weight_maint * liquidation_discount;

        if all >= I80F48::ZERO {
            thread_debug!("Account {:?} has no liquidatable amount: {:?}, asset_weight_maint: {:?}, liab_weight_maint: {:?}", account.address, all, asset_weight_maint, liab_weight_maint);
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let underwater_maint_value =
            maintenance_health / (asset_weight_maint - liab_weight_maint * liquidation_discount);

        let (asset_amount, _) = self.get_balance_for_bank(account, asset_bank_pk)?;
        let (_, liab_amount) = self.get_balance_for_bank(account, liab_bank_pk)?;

        let asset_value = asset_bank_wrapper.calc_value(
            &asset_oracle_wrapper,
            asset_amount,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_value = liab_bank_wrapper.calc_value(
            &liab_oracle_wrapper,
            liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_value = min(min(asset_value, liab_value), underwater_maint_value);
        let liquidator_profit = max_liquidatable_value * fixed_macro::types::I80F48!(0.025);

        if liquidator_profit <= self.min_profit {
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let max_liquidatable_asset_amount = asset_bank_wrapper.calc_amount(
            &asset_oracle_wrapper,
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_liab_amount = liab_bank_wrapper.calc_amount(
            &liab_oracle_wrapper,
            max_liquidatable_value,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        thread_debug!("Account {:?} liquidability evaluation:\nTotal weighted Assets {:?}\nTotal weighted Liabilities {:?}\nMaintenance health {:?}\n\
            Asset Bank {:?}\nAsset maint weight: {:?}\nAsset Amount {:?}\nAsset Value (USD) {:?}\n\
            Liab Bank {:?}\nLiab maint weight: {:?}\nLiab Amount {:?}\nLiab Value (USD) {:?}\n\
            Max Liquidatable Value {:?}\nMax Liquidatable Asset Amount {:?}\nMax Liquidatable Liab Amount {:?}\nLiquidator profit (USD) {:?}", 
            account.address, total_weighted_assets, total_weighted_liabilities, maintenance_health,
            asset_bank_wrapper.address, asset_bank_wrapper.bank.config.asset_weight_maint, asset_amount, asset_value,
            liab_bank_wrapper.address, liab_bank_wrapper.bank.config.liability_weight_maint, liab_amount, liab_value,
            max_liquidatable_value, max_liquidatable_asset_amount, max_liquidatable_liab_amount, liquidator_profit);

        Ok((
            max_liquidatable_asset_amount,
            max_liquidatable_liab_amount,
            liquidator_profit,
        ))
    }

    /// Gets the balance for a given [`MarginfiAccount`] and [`Bank`]
    // TODO: merge with `get_balance_for_bank` in `MarginfiAccountWrapper`
    fn get_balance_for_bank(
        &self,
        account: &MarginfiAccountWrapper,
        bank_pk: &Pubkey,
    ) -> Result<(I80F48, I80F48)> {
        let bank_wrapper = self
            .cache
            .banks
            .get_bank(bank_pk)
            .ok_or_else(|| anyhow!("Bank {} not bound", bank_pk))?;

        let balance = account
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == *bank_pk && b.is_active())
            .map(|b| match b.get_side()? {
                BalanceSide::Assets => {
                    let amount = bank_wrapper
                        .bank
                        .get_asset_amount(b.asset_shares.into())
                        .ok()?;
                    Some((amount, I80F48::ZERO))
                }
                BalanceSide::Liabilities => {
                    let amount = bank_wrapper
                        .bank
                        .get_liability_amount(b.liability_shares.into())
                        .ok()?;
                    Some((I80F48::ZERO, amount))
                }
            })
            .map(|e| e.unwrap_or_default())
            .unwrap_or_default();

        Ok(balance)
    }

    fn get_value_of_shares(
        &self,
        shares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for (shares_amount, bank_pk) in shares {
            let bank_wrapper = self.cache.banks.try_get_bank(&bank_pk)?;
            let oracle_wrapper = OracleWrapper::build(&self.cache, &bank_pk)?;

            if !self.support_isolated_banks
                && matches!(bank_wrapper.bank.config.risk_tier, RiskTier::Isolated)
            {
                continue;
            }

            if !matches!(
                bank_wrapper.bank.config.operational_state,
                BankOperationalState::Operational
            ) {
                continue;
            }

            // TODO: add Banks to Geyser!!!
            if bank_wrapper.bank.check_utilization_ratio().is_err() {
                thread_debug!("Skipping bankrupt bank from evaluation: {}", bank_pk);
                continue;
            }

            let value = match balance_side {
                BalanceSide::Liabilities => {
                    let liabilities = bank_wrapper
                        .bank
                        .get_liability_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate liability amount for: {}", e))?;
                    let oracle_wrapper = OracleWrapper::build(&self.cache, &bank_pk)?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        liabilities,
                        BalanceSide::Liabilities,
                        requirement_type,
                    )?
                }
                BalanceSide::Assets => {
                    let assets = bank_wrapper
                        .bank
                        .get_asset_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate asset amount for: {}", e))?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        assets,
                        BalanceSide::Assets,
                        requirement_type,
                    )?
                }
            };

            values.push((value, bank_pk));
        }

        Ok(values)
    }
}
