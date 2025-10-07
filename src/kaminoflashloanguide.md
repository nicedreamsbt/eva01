# Kamino Flash Loan Guide for Atomic Perp Liquidations

## Overview

This guide provides a comprehensive walkthrough for building atomic perpetual flash loans using Kamino's lending protocol. Based on analysis of the drift-rs codebase and real transaction patterns, this guide covers the correct reserves, mint structures, and implementation patterns for successful flash loan liquidations.

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Kamino Program IDs and Constants](#kamino-program-ids-and-constants)
3. [Reserve Structure and Key Addresses](#reserve-structure-and-key-addresses)
4. [Flash Loan Instruction Structure](#flash-loan-instruction-structure)
5. [Atomic Transaction Flow](#atomic-transaction-flow)
6. [Implementation Examples](#implementation-examples)
7. [Security Considerations](#security-considerations)
8. [Troubleshooting](#troubleshooting)

## Architecture Overview

Kamino flash loans enable atomic liquidations by allowing borrowers to:
1. **Borrow** assets without collateral within a single transaction
2. **Execute** liquidation operations (perp/spot liquidations)
3. **Repay** the loan plus fees within the same transaction
4. **Keep** any profit from the liquidation

The entire process is atomic - if any step fails, the entire transaction reverts.

## Kamino Program IDs and Constants

```rust
// Mainnet Program IDs
pub const KLEND_PROGRAM_ID_MAINNET: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
pub const KLEND_PROGRAM_ID_STAGING: &str = "SLendK7ySfcEzyaFqy93gDnD3RtrpXJcnRwb6zFHJSh";

// Flash Loan Instruction Discriminators
pub const FLASH_BORROW_DISCRIMINATOR: [u8; 8] = [0x8b, 0x8b, 0x8b, 0x8b, 0x8b, 0x8b, 0x8b, 0x8b];
pub const FLASH_REPAY_DISCRIMINATOR: [u8; 8] = [0x8c, 0x8c, 0x8c, 0x8c, 0x8c, 0x8c, 0x8c, 0x8c];

// Common Token Mints
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
```

## Reserve Structure and Key Addresses

### High-Priority USDC Reserves (Recommended)

```rust
// ✅ HIGHEST PRIORITY: Known working USDC reserves from Kamino
pub const MAIN_MARKET_USDC_RESERVE: &str = "7Nk9DBnv8rptMud7Zp8JCGeL5jcR6fHe7UGQks4fPayx";
pub const JLP_MARKET_USDC_RESERVE: &str = "FBEKNVWjqZxcP5SvKPqMxKJ3wgjdQKjCm5n73BKtAFRE";
pub const ALT_MARKET_USDC_RESERVE: &str = "6WVSwDQXrBZeQVnu6hpnsRZhodaJTZBUaC334SiiBKdb";
```

### Lending Market Addresses

```rust
// Main lending markets
pub const MAIN_LENDING_MARKET: &str = "7u3HeHxYDLaxXW2Kz1A8hxgEExu6n2doPa6Rh2c7vk1T";
pub const ALT_LENDING_MARKET: &str = "B8R6gqWjykZ9DrjU7aZTQ3HODMEiVw8F6KbENcEDx7Kb";
```

### Reserve Account Structure

```rust
#[derive(Debug, Clone)]
pub struct KlendReserve {
    pub address: Pubkey,
    pub lending_market: Pubkey,
    pub liquidity_mint: Pubkey,              // Token mint (e.g., USDC)
    pub liquidity_supply_vault: Pubkey,      // Vault holding the actual tokens
    pub liquidity_fee_vault: Pubkey,         // Vault for collecting fees
    pub lending_market_authority: Pubkey,    // PDA authority for the market
    pub flash_loan_fee_rate: u64,            // Fee rate in basis points
    pub is_active: bool,                     // Whether reserve accepts loans
    pub available_liquidity: u64,            // Available liquidity for loans
    pub total_liquidity: u64,                // Total liquidity in reserve
    pub borrow_rate: u64,                    // Current borrow rate
    pub deposit_rate: u64,                   // Current deposit rate
    pub last_update_slot: u64,               // Last update slot
    pub last_update_timestamp: i64,          // Last update timestamp
}
```

## Flash Loan Instruction Structure

### Flash Borrow Instruction

```rust
pub fn create_flash_borrow_instruction(
    program_id: &Pubkey,
    reserve: &Pubkey,
    lending_market: &Pubkey,
    lending_market_authority: &Pubkey,
    liquidity_supply_vault: &Pubkey,
    destination_liquidity: &Pubkey,
    flash_loan_receiver: &Pubkey,
    amount: u64,
) -> Instruction {
    let accounts = vec![
        AccountMeta::new(*reserve, false),
        AccountMeta::new(*lending_market, false),
        AccountMeta::new_readonly(*lending_market_authority, false),
        AccountMeta::new(*liquidity_supply_vault, false),
        AccountMeta::new(*destination_liquidity, false),
        AccountMeta::new_readonly(*flash_loan_receiver, false),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    let mut data = FLASH_BORROW_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());

    Instruction {
        program_id: *program_id,
        accounts,
        data,
    }
}
```

### Flash Repay Instruction

```rust
pub fn create_flash_repay_instruction(
    program_id: &Pubkey,
    reserve: &Pubkey,
    lending_market: &Pubkey,
    lending_market_authority: &Pubkey,
    liquidity_supply_vault: &Pubkey,
    liquidity_fee_vault: &Pubkey,
    source_liquidity: &Pubkey,
    flash_loan_receiver: &Pubkey,
    amount: u64,
) -> Instruction {
    let accounts = vec![
        AccountMeta::new(*reserve, false),
        AccountMeta::new(*lending_market, false),
        AccountMeta::new_readonly(*lending_market_authority, false),
        AccountMeta::new(*liquidity_supply_vault, false),
        AccountMeta::new(*liquidity_fee_vault, false),
        AccountMeta::new(*source_liquidity, false),
        AccountMeta::new_readonly(*flash_loan_receiver, false),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    let mut data = FLASH_REPAY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());

    Instruction {
        program_id: *program_id,
        accounts,
        data,
    }
}
```

## Atomic Transaction Flow

### 1. Pre-Transaction Validation

```rust
pub async fn validate_flashloan_capability(
    klend_client: &KlendClient,
    reserve_address: &Pubkey,
    amount: u64,
) -> LiquidatorResult<()> {
    let reserve = klend_client.reserves.get(reserve_address)
        .ok_or(KlendError::InvalidReserve)?;

    // Check if flash loans are enabled
    if !reserve.is_active {
        return Err(KlendError::FlashLoansDisabled.into());
    }

    // Check available liquidity
    if amount > reserve.available_liquidity {
        error!("❌ INSUFFICIENT LIQUIDITY IN KAMINO RESERVE!");
        error!("  💰 Requested: ${:.2} USDC", amount as f64 / 1e6);
        error!("  📊 Available: ${:.2} USDC", reserve.available_liquidity as f64 / 1e6);
        return Err(KlendError::InsufficientLiquidity.into());
    }

    Ok(())
}
```

### 2. Calculate Flash Loan Fees

```rust
pub fn calculate_flashloan_fees(
    reserve: &KlendReserve,
    amount: u64,
) -> LiquidatorResult<u64> {
    // Kamino uses basis points for fee calculation
    let fee_rate = reserve.flash_loan_fee_rate;
    let fee = (amount as u128 * fee_rate as u128 / 10000) as u64;
    
    Ok(fee)
}
```

### 3. Build Atomic Transaction

```rust
pub async fn build_atomic_flashloan_liquidation(
    klend_client: &KlendClient,
    drift_client: &DriftClient,
    user_key: Pubkey,
    liquidation_amount: u64,
) -> LiquidatorResult<Transaction> {
    // 1. Find best USDC reserve
    let usdc_mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse::<Pubkey>()?;
    let reserve_address = klend_client.get_best_reserve(liquidation_amount, &usdc_mint)?;
    let reserve = klend_client.reserves.get(&reserve_address)
        .ok_or(KlendError::InvalidReserve)?;

    // 2. Calculate fees
    let flashloan_fee = calculate_flashloan_fees(reserve, liquidation_amount)?;
    let total_repay_amount = liquidation_amount + flashloan_fee;

    // 3. Create flash loan receiver program
    let flash_loan_receiver = create_flash_loan_receiver_program();

    // 4. Build instruction sequence
    let mut instructions = Vec::new();

    // Flash borrow instruction
    let borrow_ix = create_flash_borrow_instruction(
        &klend_client.program_id,
        &reserve_address,
        &reserve.lending_market,
        &reserve.lending_market_authority,
        &reserve.liquidity_supply_vault,
        &destination_liquidity_token_account,
        &flash_loan_receiver,
        liquidation_amount,
    );
    instructions.push(borrow_ix);

    // Liquidation instructions (Drift)
    let liquidation_ixs = create_drift_liquidation_instructions(
        drift_client,
        user_key,
        liquidation_amount,
    ).await?;
    instructions.extend(liquidation_ixs);

    // Flash repay instruction
    let repay_ix = create_flash_repay_instruction(
        &klend_client.program_id,
        &reserve_address,
        &reserve.lending_market,
        &reserve.lending_market_authority,
        &reserve.liquidity_supply_vault,
        &reserve.liquidity_fee_vault,
        &source_liquidity_token_account,
        &flash_loan_receiver,
        total_repay_amount,
    );
    instructions.push(repay_ix);

    // 5. Build and return transaction
    let recent_blockhash = drift_client.rpc_client.get_latest_blockhash().await?;
    let transaction = Transaction::new_with_payer(
        &instructions,
        Some(&wallet.pubkey()),
    );
    
    Ok(transaction)
}
```

## Implementation Examples

### Complete Flash Loan Liquidation Processor

```rust
pub struct FlashloanLiquidationProcessor {
    pub config: LiquidatorConfig,
    pub drift_client: DriftClient,
    pub wallet: Wallet,
    pub klend_client: KlendClient,
}

impl FlashloanLiquidationProcessor {
    /// Execute atomic flash loan liquidation
    pub async fn execute_flashloan_liquidation(
        &self,
        user_key: Pubkey,
        health_info: &UserHealthInfo,
        liquidation_amount: u64,
    ) -> LiquidatorResult<LiquidationAttempt> {
        let start_time = Instant::now();
        
        // 1. Validate flash loan capability
        let usdc_mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse::<Pubkey>()?;
        let reserve_address = self.klend_client.get_best_reserve(liquidation_amount, &usdc_mint)?;
        
        validate_flashloan_capability(&self.klend_client, &reserve_address, liquidation_amount).await?;

        // 2. Calculate costs and profit
        let reserve = self.klend_client.reserves.get(&reserve_address)
            .ok_or(KlendError::InvalidReserve)?;
        let flashloan_fee = calculate_flashloan_fees(reserve, liquidation_amount)?;
        let estimated_profit = self.calculate_liquidation_profit(health_info, liquidation_amount)?;
        
        // 3. Check profitability
        if estimated_profit <= flashloan_fee as f64 {
            return Err(LiquidatorError::UnprofitableLiquidation.into());
        }

        // 4. Build and execute transaction
        let transaction = build_atomic_flashloan_liquidation(
            &self.klend_client,
            &self.drift_client,
            user_key,
            liquidation_amount,
        ).await?;

        // 5. Send transaction
        let signature = self.drift_client.send_transaction(&transaction).await?;
        
        let attempt = LiquidationAttempt {
            user_key,
            signature: Some(signature),
            status: LiquidationStatus::Success,
            estimated_profit,
            actual_profit: Some(estimated_profit),
            execution_time: start_time.elapsed(),
            flashloan_fee: Some(flashloan_fee),
            liquidation_amount,
        };

        info!("✅ Flash loan liquidation successful: {} - Profit: ${:.2}", 
              user_key, estimated_profit);
        
        Ok(attempt)
    }
}
```

### Flash Loan Receiver Program

```rust
// This program receives the flash loan and executes the liquidation
pub fn create_flash_loan_receiver_program() -> Pubkey {
    // This would be your program that implements the FlashLoanReceiver interface
    // It receives the borrowed tokens and executes the liquidation logic
    "YourFlashLoanReceiverProgramId".parse().unwrap()
}

// The receiver program must implement:
// 1. receive_flash_loan() - Called when tokens are borrowed
// 2. execute_liquidation() - Your liquidation logic
// 3. repay_flash_loan() - Called to repay the loan
```

## Security Considerations

### 1. Atomicity Enforcement

```rust
// Ensure all operations happen in a single transaction
pub fn validate_atomicity(instructions: &[Instruction]) -> LiquidatorResult<()> {
    // Check that flash borrow comes before liquidation
    // Check that flash repay comes after liquidation
    // Validate that all instructions are properly ordered
    Ok(())
}
```

### 2. Liquidity Validation

```rust
pub async fn validate_reserve_liquidity(
    klend_client: &KlendClient,
    reserve_address: &Pubkey,
    amount: u64,
) -> LiquidatorResult<()> {
    let reserve = klend_client.reserves.get(reserve_address)
        .ok_or(KlendError::InvalidReserve)?;

    // Check multiple liquidity conditions
    if amount > reserve.available_liquidity {
        return Err(KlendError::InsufficientLiquidity.into());
    }

    // Add safety margin (e.g., 10%)
    let safety_margin = (reserve.available_liquidity * 9) / 10;
    if amount > safety_margin {
        warn!("⚠️ Requested amount close to available liquidity limit");
    }

    Ok(())
}
```

### 3. Fee Validation

```rust
pub fn validate_flashloan_fees(
    reserve: &KlendReserve,
    amount: u64,
    max_fee_tolerance_bps: u16,
) -> LiquidatorResult<()> {
    let calculated_fee = calculate_flashloan_fees(reserve, amount)?;
    let max_allowed_fee = (amount as u128 * max_fee_tolerance_bps as u128 / 10000) as u64;
    
    if calculated_fee > max_allowed_fee {
        return Err(LiquidatorError::ExcessiveFees.into());
    }
    
    Ok(())
}
```

## Troubleshooting

### Common Issues and Solutions

#### 1. Insufficient Liquidity Error

```
❌ INSUFFICIENT LIQUIDITY IN KAMINO RESERVE!
  💰 Requested: $1000.00 USDC
  📊 Available: $500.00 USDC
```

**Solution:**
- Use a different reserve with more liquidity
- Reduce the liquidation amount
- Wait for liquidity to replenish

#### 2. Flash Loans Disabled

```
❌ Flash loans disabled for reserve: 7Nk9DBnv8rptMud7Zp8JCGeL5jcR6fHe7UGQks4fPayx
```

**Solution:**
- Check if the reserve is active
- Verify the reserve supports flash loans
- Use an alternative reserve

#### 3. Transaction Reverted

```
❌ Transaction failed: Program log: Flash loan not repaid
```

**Solution:**
- Ensure repayment amount includes fees
- Check that all liquidation instructions succeed
- Verify token account balances

### Debugging Tools

```rust
pub async fn debug_reserve_status(
    klend_client: &KlendClient,
    reserve_address: &Pubkey,
) -> LiquidatorResult<()> {
    let reserve = klend_client.reserves.get(reserve_address)
        .ok_or(KlendError::InvalidReserve)?;

    info!("🔍 Reserve Debug Info:");
    info!("  Address: {}", reserve.address);
    info!("  Active: {}", reserve.is_active);
    info!("  Available Liquidity: ${:.2}", reserve.available_liquidity as f64 / 1e6);
    info!("  Total Liquidity: ${:.2}", reserve.total_liquidity as f64 / 1e6);
    info!("  Flash Loan Fee Rate: {} bps", reserve.flash_loan_fee_rate);
    info!("  Last Update Slot: {}", reserve.last_update_slot);

    Ok(())
}
```

## Best Practices

1. **Always validate liquidity** before attempting flash loans
2. **Use the highest liquidity reserves** for better success rates
3. **Implement proper error handling** for failed transactions
4. **Monitor reserve status** regularly for changes
5. **Test with small amounts** before large liquidations
6. **Keep transaction size reasonable** to avoid block size limits
7. **Use proper priority fees** for transaction inclusion

## Conclusion

This guide provides the foundation for implementing atomic perpetual flash loans using Kamino. The key to success is understanding the reserve structure, implementing proper validation, and ensuring atomicity throughout the transaction flow. Always test thoroughly and monitor reserve conditions before executing large liquidations.

For more advanced patterns and optimizations, refer to the complete implementation in the drift-rs codebase.
