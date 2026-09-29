use soroban_sdk::{contracttype, symbol_short, Address, Env, IntoVal, Symbol};

use crate::ContractError;

/// Basis-point denominator used by collateral ratios.
pub const BPS_DENOMINATOR: u128 = 10_000;
/// A vault is eligible for liquidation below 110% collateralization.
pub const DEFAULT_LIQUIDATION_THRESHOLD_BPS: u32 = 11_000;
/// Floor bonus: a liquidator receives at least this share of the confiscated
/// collateral, even for a position sitting just below the threshold.
pub const LIQUIDATOR_BONUS_BPS: u32 = 500;

/// Extra bonus that scales in as the position degrades, reached in full at a
/// health factor of zero.
pub const MAX_LIQUIDATOR_DEGRADATION_BPS: u32 = 2_000;

/// Hard ceiling on the liquidator's total share, so a bonus can never consume
/// the whole position and leave the protocol reserve empty.
pub const MAX_TOTAL_LIQUIDATOR_BPS: u32 = 5_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultPosition {
    pub owner: Address,
    /// Collateral amount, or its value when prices have already been applied.
    pub collateral_value: u128,
    /// Configured liquidation threshold in basis points. Zero uses 110%.
    pub liquidation_threshold_bps: u32,
    /// Debt amount, or its value when prices have already been applied.
    pub borrowed_value: u128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiquidationResult {
    pub liquidated: bool,
    /// Collateralization ratio in basis points (10_000 == 100%).
    pub health_factor: u128,
    pub liquidator_reward: u128,
    pub protocol_reserve: u128,
}

pub fn health_factor(position: &VaultPosition) -> Result<u128, ContractError> {
    if position.borrowed_value == 0 {
        return Ok(u128::MAX);
    }

    position
        .collateral_value
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(ContractError::MathOverflow)?
        .checked_div(position.borrowed_value)
        .ok_or(ContractError::DivisionByZero)
}

fn threshold(position: &VaultPosition) -> u128 {
    if position.liquidation_threshold_bps == 0 {
        DEFAULT_LIQUIDATION_THRESHOLD_BPS as u128
    } else {
        position.liquidation_threshold_bps as u128
    }
}

/// Liquidator share for a position, in basis points (#932).
///
/// `P_liq = P_base + (1 - H) * P_bonus`
///
/// `health_factor_bps` is the ratio scaled by [`BPS_DENOMINATOR`], so 10_000 is
/// 100% health. The degradation term is guarded rather than computed blindly:
/// health can exceed 10_000 bps and still be liquidatable, because a position
/// at 109% is under a 110% threshold, and an unsigned subtraction there would
/// wrap to an enormous number.
pub fn scaled_liquidator_bonus_bps(health_factor_bps: u128) -> u128 {
    let base = LIQUIDATOR_BONUS_BPS as u128;

    let degraded = if health_factor_bps >= BPS_DENOMINATOR {
        0
    } else {
        ((BPS_DENOMINATOR - health_factor_bps) * MAX_LIQUIDATOR_DEGRADATION_BPS as u128)
            / BPS_DENOMINATOR
    };

    core::cmp::min(base + degraded, MAX_TOTAL_LIQUIDATOR_BPS as u128)
}

pub fn liquidate(
    _env: &Env,
    position: &VaultPosition,
    purchase_collateral: u128,
) -> Result<LiquidationResult, ContractError> {
    let hf = health_factor(position)?;
    if hf >= threshold(position) {
        return Ok(LiquidationResult {
            liquidated: false,
            health_factor: hf,
            liquidator_reward: 0,
            protocol_reserve: 0,
        });
    }

    // The deeper underwater the position is, the larger the incentive a
    // liquidator gets for taking it off the books.
    let bonus_bps = scaled_liquidator_bonus_bps(hf);

    let reward = purchase_collateral
        .checked_mul(bonus_bps)
        .ok_or(ContractError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR)
        .ok_or(ContractError::DivisionByZero)?;
    let protocol_reserve = purchase_collateral
        .checked_sub(reward)
        .ok_or(ContractError::MathOverflow)?;

    Ok(LiquidationResult {
        liquidated: true,
        health_factor: hf,
        liquidator_reward: reward,
        protocol_reserve,
    })
}

/// Zero-loss invariant for the liquidation ledger (#925).
///
/// Confiscated collateral is only ever split, never created or destroyed, so
/// the liquidator reward plus the protocol reserve must equal exactly the
/// collateral that entered the split. A non-liquidation must move nothing at
/// all, otherwise a rejected liquidation would still book a transfer.
pub fn conserves_collateral(result: &LiquidationResult, purchase_collateral: u128) -> bool {
    if !result.liquidated {
        return result.liquidator_reward == 0 && result.protocol_reserve == 0;
    }

    result
        .liquidator_reward
        .checked_add(result.protocol_reserve)
        .map(|sum| sum == purchase_collateral)
        .unwrap_or(false)
}

/// Price a vault using the oracle's verified `get_twap(Symbol)` feed before
/// applying the liquidation rule. Missing, stale, or invalid feeds fail
/// closed; a caller cannot provide a fabricated price.
pub fn liquidate_at_twap(
    env: &Env,
    oracle: &Address,
    collateral_asset: &Symbol,
    debt_asset: &Symbol,
    position: &VaultPosition,
    purchase_collateral: u128,
) -> Result<LiquidationResult, ContractError> {
    let collateral_price = read_twap(env, oracle, collateral_asset)?;
    let debt_price = read_twap(env, oracle, debt_asset)?;
    if collateral_price <= 0 || debt_price <= 0 {
        return Err(ContractError::NotInitialized);
    }

    let collateral_value = position
        .collateral_value
        .checked_mul(collateral_price as u128)
        .ok_or(ContractError::MathOverflow)?;
    let borrowed_value = position
        .borrowed_value
        .checked_mul(debt_price as u128)
        .ok_or(ContractError::MathOverflow)?;
    let priced_position = VaultPosition {
        collateral_value,
        borrowed_value,
        ..position.clone()
    };

    liquidate(env, &priced_position, purchase_collateral)
}

fn read_twap(env: &Env, oracle: &Address, asset: &Symbol) -> Result<i128, ContractError> {
    let result: Result<Option<i128>, soroban_sdk::Error> = env.invoke_contract(
        oracle,
        &symbol_short!("get_twap"),
        soroban_sdk::vec![env, asset.into_val(env)],
    );
    match result {
        Ok(Some(price)) => Ok(price),
        _ => Err(ContractError::NotInitialized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    fn position(env: &Env, collateral: u128, debt: u128) -> VaultPosition {
        VaultPosition {
            owner: Address::generate(env),
            collateral_value: collateral,
            liquidation_threshold_bps: DEFAULT_LIQUIDATION_THRESHOLD_BPS,
            borrowed_value: debt,
        }
    }

    #[test]
    fn calculates_ratio_without_integer_truncation() {
        let env = Env::default();
        assert_eq!(health_factor(&position(&env, 109, 100)).unwrap(), 10_900);
        assert_eq!(health_factor(&position(&env, 110, 100)).unwrap(), 11_000);
    }

    #[test]
    fn liquidates_below_110_percent_and_splits_five_percent_bonus() {
        let env = Env::default();
        let result = liquidate(&env, &position(&env, 109, 100), 100).unwrap();
        assert!(result.liquidated);
        assert_eq!(result.health_factor, 10_900);
        assert_eq!(result.liquidator_reward, 5);
        assert_eq!(result.protocol_reserve, 95);
    }

    #[test]
    fn does_not_liquidate_at_or_above_threshold() {
        let env = Env::default();
        let result = liquidate(&env, &position(&env, 110, 100), 100).unwrap();
        assert!(!result.liquidated);
        assert_eq!(result.liquidator_reward, 0);
    }

    #[test]
    fn penalty_is_flat_at_or_above_full_health() {
        // A position just under a 110% threshold is not "degraded" in the
        // formula's terms, so it earns the floor bonus only.
        assert_eq!(scaled_liquidator_bonus_bps(10_900), 500);
        assert_eq!(scaled_liquidator_bonus_bps(10_000), 500);
        // Unsigned wraparound guard: health far above 100% must not invert.
        assert_eq!(scaled_liquidator_bonus_bps(u128::MAX), 500);
    }

    #[test]
    fn penalty_grows_as_health_degrades() {
        // 50% health -> half of the 2000 bps degradation term.
        assert_eq!(scaled_liquidator_bonus_bps(5_000), 500 + 1_000);
        // 25% health -> a quarter of the term.
        assert_eq!(scaled_liquidator_bonus_bps(2_500), 500 + 1_500);
        // Zero health -> the full term, still under the ceiling.
        assert_eq!(scaled_liquidator_bonus_bps(0), 500 + 2_000);
    }

    #[test]
    fn penalty_never_exceeds_the_ceiling() {
        assert!(scaled_liquidator_bonus_bps(0) <= MAX_TOTAL_LIQUIDATOR_BPS as u128);
        assert_eq!(MAX_TOTAL_LIQUIDATOR_BPS, 5_000);
    }

    #[test]
    fn deeper_positions_pay_liquidators_more() {
        let env = Env::default();
        let shallow = liquidate(&env, &position(&env, 109, 100), 1_000).unwrap();
        let deep = liquidate(&env, &position(&env, 40, 100), 1_000).unwrap();

        assert!(shallow.liquidated && deep.liquidated);
        assert!(
            deep.liquidator_reward > shallow.liquidator_reward,
            "expected a deeper position to pay more: {} vs {}",
            deep.liquidator_reward,
            shallow.liquidator_reward
        );
    }

    #[test]
    fn liquidation_split_conserves_all_collateral() {
        // Zero-loss invariant across a spread of health factors and sizes.
        let env = Env::default();
        for collateral in [0u128, 1, 7, 999, 1_000_000] {
            for health in [10_999u128, 10_900, 9_000, 5_000, 1, 0] {
                let result = liquidate(&env, &position(&env, health, 100), collateral).unwrap();
                assert!(
                    conserves_collateral(&result, collateral),
                    "collateral leaked at health={} size={}: reward={} reserve={}",
                    health,
                    collateral,
                    result.liquidator_reward,
                    result.protocol_reserve
                );
            }
        }
    }

    #[test]
    fn rejected_liquidation_moves_nothing() {
        let env = Env::default();
        let result = liquidate(&env, &position(&env, 200, 100), 1_000).unwrap();
        assert!(!result.liquidated);
        assert!(conserves_collateral(&result, 1_000));
    }
}
