use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    cache::Cache,
    cli::setup::marginfi_account_by_authority,
    config::GeneralConfig,
    marginfi_ixs::{
        initialize_marginfi_account, make_deposit_ix, make_end_flashloan_ix, make_liquidate_ix,
        make_repay_ix, make_start_flashloan_ix, make_withdraw_ix,
    },
    metrics::LIQUIDATION_ATTEMPTS,
    thread_debug, thread_info, thread_warn,
    utils::{check_asset_tags_matching, swb_cranker::is_stale_swb_price_error},
    wrappers::oracle::OracleWrapper,
};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use marginfi_type_crate::types::BalanceSide;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};

use crate::wrappers::oracle::OracleWrapperTrait;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, CompileError, VersionedMessage},
    pubkey,
    signature::Keypair,
    signer::{Signer, SignerError},
    system_instruction::transfer,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::runtime::Builder;

#[derive(Debug)]
pub struct LiquidationError {
    pub error: anyhow::Error,
    pub keys: Vec<Pubkey>,
}

impl LiquidationError {
    pub fn from_anyhow_error(error: anyhow::Error) -> Self {
        Self {
            error,
            keys: vec![],
        }
    }

    pub fn from_anyhow_error_with_keys(error: anyhow::Error, keys: Vec<Pubkey>) -> Self {
        Self { error, keys }
    }

    pub fn from_compile_error(error: CompileError) -> Self {
        Self {
            error: anyhow!("{:?}", error),
            keys: vec![],
        }
    }

    pub fn from_signer_error(error: SignerError) -> Self {
        Self {
            error: anyhow!("{:?}", error),
            keys: vec![],
        }
    }
}

pub struct LiquidatorAccount {
    pub liquidator_address: Pubkey,
    pub signer: Keypair,
    program_id: Pubkey,
    group: Pubkey,
    preferred_mint_bank: Pubkey,
    rpc_client: RpcClient,
    cu_limit_ix: Instruction,
    cu_price_ix: Instruction,
    flashloan_enabled: bool,
    jup_swap_api_url: String,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: ComputeUnitPriceMicroLamports,
    jup_lut_cache: Arc<Mutex<HashMap<Pubkey, AddressLookupTableAccount>>>,
    pub cache: Arc<Cache>,
}

impl LiquidatorAccount {
    pub fn new(
        config: &GeneralConfig,
        marginfi_group_id: Pubkey,
        preferred_mint: Pubkey,
        jup_swap_api_url: String,
        slippage_bps: u16,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let signer = Keypair::from_bytes(&config.wallet_keypair)?;
        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());

        let accounts = marginfi_account_by_authority(
            signer.pubkey(),
            &rpc_client,
            config.marginfi_program_id,
            marginfi_group_id,
        )?;
        thread_info!(
            "Found {} MarginFi accounts for the provided signer: {:?}",
            accounts.len(),
            accounts
        );

        let liquidator_address = if accounts.is_empty() {
            thread_info!("No MarginFi account found for the provided signer. Creating it...");
            let liquidator_marginfi_account = initialize_marginfi_account(
                &rpc_client,
                config.marginfi_program_id,
                marginfi_group_id,
                &signer,
            )?;

            while cache
                .marginfi_accounts
                .try_get_account(&liquidator_marginfi_account)
                .is_err()
            {
                thread_info!("Waiting for the new account info to arrive...");
                thread::sleep(Duration::from_secs(5));
            }

            liquidator_marginfi_account
        } else {
            // Prefer the funded account: CUANYWWJX3J2CPZNRKyzq7iV2zpETipG2VntG3P6kH5P
            let funded_account =
                Pubkey::from_str("CUANYWWJX3J2CPZNRKyzq7iV2zpETipG2VntG3P6kH5P").unwrap();
            if accounts.contains(&funded_account) {
                thread_info!("Using funded account: {}", funded_account);
                funded_account
            } else {
                thread_info!("Using first available account: {}", accounts[0]);
                accounts[0]
            }
        };

        let preferred_mint_bank = cache.banks.try_get_account_for_mint(&preferred_mint)?;
        let jup_lut_cache = cache
            .luts
            .iter()
            .cloned()
            .map(|lut| (lut.key, lut))
            .collect::<HashMap<_, _>>();

        Ok(Self {
            liquidator_address,
            signer,
            program_id: config.marginfi_program_id,
            group: marginfi_group_id,
            preferred_mint_bank,
            rpc_client,
            cu_limit_ix: ComputeBudgetInstruction::set_compute_unit_limit(
                config.compute_unit_limit,
            ),
            cu_price_ix: ComputeBudgetInstruction::set_compute_unit_price(
                config.compute_unit_price_micro_lamports,
            ),
            flashloan_enabled: config.flashloan_liquidation,
            jup_swap_api_url,
            slippage_bps,
            compute_unit_price_micro_lamports: ComputeUnitPriceMicroLamports::MicroLamports(
                config.compute_unit_price_micro_lamports,
            ),
            jup_lut_cache: Arc::new(Mutex::new(jup_lut_cache)),
            cache,
        })
    }

    pub fn has_funds(&self) -> Result<bool> {
        let account = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)?;

        let preferred_mint_bank = self.cache.banks.try_get_bank(&self.preferred_mint_bank)?;

        let validation_result =
            account
                .get_balance_for_bank(&preferred_mint_bank)
                .map(|(balance, side)| match side {
                    BalanceSide::Assets => balance > 0,
                    _ => true,
                });

        Ok(validation_result.unwrap_or(false))
    }

    pub fn flashloan_enabled(&self) -> bool {
        self.flashloan_enabled
    }

    pub fn liquidate(
        &self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
        liab_amount: u64,
        stale_swb_oracles: &HashSet<Pubkey>,
    ) -> Result<(), LiquidationError> {
        if self.flashloan_enabled {
            return self.liquidate_with_flashloan(
                liquidatee_account,
                asset_bank,
                liab_bank,
                asset_amount,
                liab_amount,
                stale_swb_oracles,
            );
        }
        self.liquidate_with_prefunded_capital(
            liquidatee_account,
            asset_bank,
            liab_bank,
            asset_amount,
            liab_amount,
            stale_swb_oracles,
        )
    }

    fn liquidate_with_prefunded_capital(
        &self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
        liab_amount: u64,
        stale_swb_oracles: &HashSet<Pubkey>,
    ) -> Result<(), LiquidationError> {
        let liquidatee_account_address = liquidatee_account.address;
        thread_info!(
            "Liquidating account {:?} with liquidator account {:?}. Amount: {}",
            liquidatee_account_address,
            self.liquidator_address,
            asset_amount
        );

        let asset_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let liab_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let signer_pk = self.signer.pubkey();
        let liab_mint = liab_bank_wrapper.bank.mint;

        let liquidator_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)
            .map_err(LiquidationError::from_anyhow_error)?;

        // TODO: remove
        if *liab_bank == Pubkey::from_str_const("BeNBJrAh1tZg5sqgt8D6AWKJLD5KkBrfZvtcgd7EuiAR") {
            let uxd_balance = liquidator_account
                .get_balance_for_bank(&liab_bank_wrapper)
                .map(|(value, _)| value.to_num())
                .unwrap_or(0);
            if uxd_balance < liab_amount {
                thread_info!("Not enough UXD collateral: ignoring liquidation");
                return Ok(());
            }
        }

        let lending_account = &liquidator_account.lending_account;

        let banks_to_include: Vec<Pubkey> = vec![*liab_bank, *asset_bank];

        for bank_pk in banks_to_include.iter() {
            let bank_to_validate_against = self
                .cache
                .banks
                .try_get_bank(bank_pk)
                .map_err(LiquidationError::from_anyhow_error)?;
            if !check_asset_tags_matching(&bank_to_validate_against.bank, lending_account) {
                // This is a precaution to not attempt to liquidate staked collateral positions when liquidator has non-SOL positions open.
                // Expected to happen quite often for now. Later on, we can add a more sophisticated filtering logic on the higher level.
                thread_debug!("Bank {:?} does not match the asset tags of the lending account -> skipping liquidation attempt", bank_pk);
                return Ok(());
            }
        }

        LIQUIDATION_ATTEMPTS.inc();

        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidator_observation_accounts, liquidator_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        thread_debug!(
            "The Liquidator {} observation accounts: {:?}",
            &self.liquidator_address,
            liquidator_observation_accounts
        );

        if contains_stale_oracles(stale_swb_oracles, &liquidator_swb_oracles) {
            thread_warn!("Skipping liquidation attempt because liquidator has stale oracles.");
            return Ok(());
        }

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidatee_observation_accounts, liquidatee_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &liquidatee_account.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        thread_debug!(
            "The Liquidatee {:?} observation accounts: {:?}",
            liquidatee_account_address,
            liquidatee_observation_accounts
        );

        if contains_stale_oracles(stale_swb_oracles, &liquidatee_swb_oracles) {
            thread_warn!("Skipping liquidation attempt because liquidatee has stale oracles.");
            return Ok(());
        }

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .copied()
            .collect::<Vec<_>>();

        let total_observation_accounts = joined_observation_accounts.len();

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            &asset_bank_wrapper,
            asset_oracle_wrapper.address,
            &liab_bank_wrapper,
            liab_oracle_wrapper.address,
            signer_pk,
            liquidatee_account_address,
            self.cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            joined_observation_accounts,
            asset_amount,
        );

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        // Use LUTs only when your transaction involves a large number of observation accounts.
        let luts: &Vec<AddressLookupTableAccount> = {
            if total_observation_accounts > 22 {
                thread_debug!(
                    "Using LUT for liquidating the Account {} .",
                    liquidatee_account_address
                );
                &self.cache.luts
            } else {
                &vec![]
            }
        };

        let msg = Message::try_compile(
            &signer_pk,
            &[self.cu_limit_ix.clone(), liquidate_ix.clone()],
            luts,
            recent_blockhash,
        )
        .map_err(LiquidationError::from_compile_error)?;

        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(LiquidationError::from_signer_error)?;

        thread_info!(
            "Sending liquidation txn for the Account {} .",
            liquidatee_account_address
        );
        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                thread_info!(
                    "Liquidation txn for the Account {} was confirmed. Signature: {}",
                    liquidatee_account_address,
                    signature,
                );
                Ok(())
            }
            Err(err) => {
                let mut swb_oracles: Vec<Pubkey> = vec![];
                if is_stale_swb_price_error(&err) {
                    swb_oracles = liquidator_swb_oracles;
                    for swb_oracle in liquidatee_swb_oracles.into_iter() {
                        if !swb_oracles.contains(&swb_oracle) {
                            swb_oracles.push(swb_oracle);
                        }
                    }
                }
                Err(LiquidationError::from_anyhow_error_with_keys(
                    anyhow!(
                        "Liquidation txn for the Account {} failed: {} ",
                        liquidatee_account_address,
                        err
                    ),
                    swb_oracles,
                ))
            }
        }
    }

    fn liquidate_with_flashloan(
        &self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
        liab_amount: u64,
        stale_swb_oracles: &HashSet<Pubkey>,
    ) -> Result<(), LiquidationError> {
        let liquidatee_account_address = liquidatee_account.address;
        thread_info!(
            "Flashloan liquidation for account {:?} with liquidator {:?}. Asset amount: {}, liab amount: {}",
            liquidatee_account_address,
            self.liquidator_address,
            asset_amount,
            liab_amount
        );

        let asset_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let liab_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let signer_pk = self.signer.pubkey();
        let liab_mint = liab_bank_wrapper.bank.mint;
        let asset_mint = asset_bank_wrapper.bank.mint;

        let liquidator_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)
            .map_err(LiquidationError::from_anyhow_error)?;
        let lending_account = &liquidator_account.lending_account;

        for bank_pk in [*liab_bank, *asset_bank] {
            let bank_to_validate_against = self
                .cache
                .banks
                .try_get_bank(&bank_pk)
                .map_err(LiquidationError::from_anyhow_error)?;
            if !check_asset_tags_matching(&bank_to_validate_against.bank, lending_account) {
                thread_debug!(
                    "Bank {:?} does not match asset tags for flashloan liquidation -> skipping",
                    bank_pk
                );
                return Ok(());
            }
        }

        LIQUIDATION_ATTEMPTS.inc();

        let (liquidator_observation_accounts, liquidator_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                lending_account,
                &[*liab_bank, *asset_bank],
                &[],
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;

        if contains_stale_oracles(stale_swb_oracles, &liquidator_swb_oracles) {
            thread_warn!("Skipping flashloan liquidation: liquidator has stale oracles.");
            return Ok(());
        }

        let (liquidatee_observation_accounts, liquidatee_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &liquidatee_account.lending_account,
                &[],
                &[],
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;

        if contains_stale_oracles(stale_swb_oracles, &liquidatee_swb_oracles) {
            thread_warn!("Skipping flashloan liquidation: liquidatee has stale oracles.");
            return Ok(());
        }

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .copied()
            .collect::<Vec<_>>();

        let liab_token_account = self
            .cache
            .tokens
            .try_get_token_for_mint(&liab_mint)
            .map_err(LiquidationError::from_anyhow_error)?;
        let asset_token_account = self
            .cache
            .tokens
            .try_get_token_for_mint(&asset_mint)
            .map_err(LiquidationError::from_anyhow_error)?;

        let borrow_liab_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            signer_pk,
            &liab_bank_wrapper,
            liab_token_account,
            self.cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            liquidator_observation_accounts.clone(),
            liab_amount,
            Some(false),
        );

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            &asset_bank_wrapper,
            asset_oracle_wrapper.address,
            &liab_bank_wrapper,
            liab_oracle_wrapper.address,
            signer_pk,
            liquidatee_account_address,
            self.cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            joined_observation_accounts.clone(),
            asset_amount,
        );

        let withdraw_asset_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            signer_pk,
            &asset_bank_wrapper,
            asset_token_account,
            self.cache
                .mints
                .try_get_account(&asset_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            liquidator_observation_accounts.clone(),
            asset_amount,
            Some(false),
        );

        let tokio_rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;
        let jup_client = JupiterSwapApiClient::new(self.jup_swap_api_url.clone());
        let quote = tokio_rt
            .block_on(jup_client.quote(&QuoteRequest {
                input_mint: asset_mint,
                output_mint: liab_mint,
                amount: asset_amount,
                slippage_bps: self.slippage_bps,
                ..Default::default()
            }))
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        let swap_instructions = tokio_rt
            .block_on(jup_client.swap_instructions(&SwapRequest {
                user_public_key: signer_pk,
                quote_response: quote,
                config: TransactionConfig {
                    wrap_and_unwrap_sol: false,
                    compute_unit_price_micro_lamports: Some(
                        self.compute_unit_price_micro_lamports.clone(),
                    ),
                    ..Default::default()
                },
            }))
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        let repay_flashloan_ix = make_repay_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            signer_pk,
            &liab_bank_wrapper,
            liab_token_account,
            self.cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            u64::MAX,
            Some(true),
        );

        let mut core_ixs = vec![
            borrow_liab_ix,
            liquidate_ix,
            withdraw_asset_ix,
            repay_flashloan_ix,
        ];
        core_ixs.splice(3..3, swap_instructions.setup_instructions.clone());
        core_ixs.insert(3, swap_instructions.swap_instruction.clone());
        if let Some(cleanup_ix) = swap_instructions.cleanup_instruction.clone() {
            core_ixs.insert(4 + swap_instructions.setup_instructions.len(), cleanup_ix);
        }
        if let Some(token_ledger_ix) = swap_instructions.token_ledger_instruction.clone() {
            core_ixs.insert(3, token_ledger_ix);
        }
        for extra_ix in swap_instructions.other_instructions.clone() {
            core_ixs.push(extra_ix);
        }

        let start_flashloan_ix = make_start_flashloan_ix(
            self.program_id,
            self.liquidator_address,
            signer_pk,
            (core_ixs.len() + 1) as u64,
        );
        let end_flashloan_ix = make_end_flashloan_ix(
            self.program_id,
            self.liquidator_address,
            signer_pk,
            liquidator_observation_accounts,
        );

        let mut all_ixs = vec![
            self.cu_limit_ix.clone(),
            self.cu_price_ix.clone(),
            start_flashloan_ix,
        ];
        all_ixs.extend(core_ixs);
        all_ixs.push(end_flashloan_ix);

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        let mut luts: Vec<AddressLookupTableAccount> = self.cache.luts.clone();
        if !swap_instructions.address_lookup_table_addresses.is_empty() {
            let mut cached_luts = self
                .load_jupiter_luts(&swap_instructions.address_lookup_table_addresses)
                .map_err(LiquidationError::from_anyhow_error)?;
            luts.append(&mut cached_luts);
        }

        let msg = Message::try_compile(&signer_pk, &all_ixs, &luts, recent_blockhash)
            .map_err(LiquidationError::from_compile_error)?;
        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(LiquidationError::from_signer_error)?;

        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                thread_info!(
                    "Flashloan liquidation txn for account {} confirmed. Signature: {}",
                    liquidatee_account_address,
                    signature
                );
                Ok(())
            }
            Err(err) => {
                let mut swb_oracles: Vec<Pubkey> = vec![];
                if is_stale_swb_price_error(&err) {
                    swb_oracles = liquidator_swb_oracles;
                    for swb_oracle in liquidatee_swb_oracles {
                        if !swb_oracles.contains(&swb_oracle) {
                            swb_oracles.push(swb_oracle);
                        }
                    }
                }
                Err(LiquidationError::from_anyhow_error_with_keys(
                    anyhow!(
                        "Flashloan liquidation txn for account {} failed: {}",
                        liquidatee_account_address,
                        err
                    ),
                    swb_oracles,
                ))
            }
        }
    }

    fn load_jupiter_luts(
        &self,
        lut_addresses: &[Pubkey],
    ) -> Result<Vec<AddressLookupTableAccount>> {
        let mut cache_guard = self
            .jup_lut_cache
            .lock()
            .map_err(|e| anyhow!("Failed to lock Jupiter LUT cache: {:?}", e))?;

        let mut result = Vec::with_capacity(lut_addresses.len());
        let mut missing = Vec::new();
        for lut_key in lut_addresses {
            if let Some(lut) = cache_guard.get(lut_key) {
                result.push(lut.clone());
            } else {
                missing.push(*lut_key);
            }
        }

        if !missing.is_empty() {
            let fetched = self.rpc_client.get_multiple_accounts(&missing)?;
            for (lut_key, lut_account_opt) in missing.into_iter().zip(fetched.into_iter()) {
                if let Some(lut_account) = lut_account_opt {
                    if let Ok(lut_state) =
                        solana_sdk::address_lookup_table::state::AddressLookupTable::deserialize(
                            &lut_account.data,
                        )
                    {
                        let lut = AddressLookupTableAccount {
                            key: lut_key,
                            addresses: lut_state.addresses.to_vec(),
                        };
                        cache_guard.insert(lut_key, lut.clone());
                        result.push(lut);
                    }
                }
            }
        }

        Ok(result)
    }

    pub fn withdraw(
        &self,
        bank: &BankWrapper,
        amount: u64,
        withdraw_all: Option<bool>,
    ) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![bank.address]
        } else {
            vec![]
        };
        thread_debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (observation_accounts, _) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &self
                    .cache
                    .marginfi_accounts
                    .try_get_account(&self.liquidator_address)?
                    .lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )?;

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            observation_accounts,
            amount,
            withdraw_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[self.cu_limit_ix.clone(), withdraw_ix],
                Some(&signer_pk),
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!(
            "Withdrawing {:?} unscaled tokens of the Mint {} from the Liquidator account {:?}, Bank {:?}, ",
            amount,
            mint,
            token_account,
            self.preferred_mint_bank
        );

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        thread_debug!("Withdrawal txn: {:?} ", res);
        Ok(())
    }

    pub fn repay(&self, bank: &BankWrapper, amount: u64, repay_all: Option<bool>) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
            repay_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[repay_ix.clone()],
                Some(&signer_pk),
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!(
            "Repaying {:?} unscaled tokens to the bank {}, token account {:?}",
            amount,
            bank.address,
            token_account
        );

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            );
        thread_debug!(
            "The repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }

    pub fn deposit(&self, bank: &BankWrapper, amount: u64) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let instructions: Vec<Instruction> =
            if mint == pubkey!("So11111111111111111111111111111111111111112") {
                vec![transfer(&signer_pk, &token_account, amount), deposit_ix]
            } else {
                vec![deposit_ix]
            };

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &instructions,
                Some(&signer_pk),
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!("Depositing {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            );
        thread_debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }
}

fn contains_stale_oracles(stale_oracles: &HashSet<Pubkey>, account_oracles: &[Pubkey]) -> bool {
    if let Some(oracle) = account_oracles
        .iter()
        .find(|oracle| stale_oracles.contains(*oracle))
    {
        thread_warn!("Found stale oracle: {}.", oracle);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_stale_oracles_with_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), stale_oracle, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_without_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), Pubkey::new_unique()];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_account_oracles() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_stale_oracles() {
        let account_oracles = vec![Pubkey::new_unique()];
        let stale_oracles = HashSet::new();

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_multiple_stale() {
        let stale1 = Pubkey::new_unique();
        let stale2 = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale1);
        stale_oracles.insert(stale2);
        let account_oracles = vec![stale2, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }
}
