use anchor_lang::prelude::*;
use anchor_spl::memo::Memo;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::swap_with_transfer_fee_extension;
use crate::util::{
    calculate_transfer_fee_excluded_amount, parse_remaining_accounts,
    update_and_two_hop_swap_pool_v2, AccountsType, RemainingAccountsInfo,
};
use crate::{
    constants::transfer_memo,
    errors::ErrorCode,
    state::{Pool, TickArray},
    util::{to_timestamp_u64, SwapTickSequence},
};

#[derive(Accounts)]
#[instruction(
    amount: u64,
    other_amount_threshold: u64,
    amount_specified_is_input: bool,
    a_to_b_one: bool,
    a_to_b_two: bool,
)]
pub struct TwoHopSwapV2<'info> {
    #[account(mut)]
    pub pool_one: Box<Account<'info, Pool>>,
    #[account(mut)]
    pub pool_two: Box<Account<'info, Pool>>,

    #[account(address = pool_one.input_token_mint(a_to_b_one))]
    pub token_mint_input: InterfaceAccount<'info, Mint>,
    #[account(address = pool_one.output_token_mint(a_to_b_one))]
    pub token_mint_intermediate: InterfaceAccount<'info, Mint>,
    #[account(address = pool_two.output_token_mint(a_to_b_two))]
    pub token_mint_output: InterfaceAccount<'info, Mint>,

    #[account(address = token_mint_input.to_account_info().owner.clone())]
    pub token_program_input: Interface<'info, TokenInterface>,
    #[account(address = token_mint_intermediate.to_account_info().owner.clone())]
    pub token_program_intermediate: Interface<'info, TokenInterface>,
    #[account(address = token_mint_output.to_account_info().owner.clone())]
    pub token_program_output: Interface<'info, TokenInterface>,

    #[account(mut, constraint = token_owner_account_input.mint == token_mint_input.key())]
    pub token_owner_account_input: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = pool_one.input_token_vault(a_to_b_one))]
    pub token_vault_one_input: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = pool_one.output_token_vault(a_to_b_one))]
    pub token_vault_one_intermediate: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, address = pool_two.input_token_vault(a_to_b_two))]
    pub token_vault_two_intermediate: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = pool_two.output_token_vault(a_to_b_two))]
    pub token_vault_two_output: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = token_owner_account_output.mint == token_mint_output.key())]
    pub token_owner_account_output: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_authority: Signer<'info>,

    #[account(mut, constraint = tick_array_one_0.load()?.pool == pool_one.key())]
    pub tick_array_one_0: AccountLoader<'info, TickArray>,

    #[account(mut, constraint = tick_array_one_1.load()?.pool == pool_one.key())]
    pub tick_array_one_1: AccountLoader<'info, TickArray>,

    #[account(mut, constraint = tick_array_one_2.load()?.pool == pool_one.key())]
    pub tick_array_one_2: AccountLoader<'info, TickArray>,

    #[account(mut, constraint = tick_array_two_0.load()?.pool == pool_two.key())]
    pub tick_array_two_0: AccountLoader<'info, TickArray>,

    #[account(mut, constraint = tick_array_two_1.load()?.pool == pool_two.key())]
    pub tick_array_two_1: AccountLoader<'info, TickArray>,

    #[account(mut, constraint = tick_array_two_2.load()?.pool == pool_two.key())]
    pub tick_array_two_2: AccountLoader<'info, TickArray>,

    pub memo_program: Program<'info, Memo>,
    // remaining accounts
    // - accounts for transfer hook program of token_mint_input
    // - accounts for transfer hook program of token_mint_intermediate
    // - accounts for transfer hook program of token_mint_output
}

pub fn handler<'a, 'b, 'c, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, TwoHopSwapV2<'info>>,
    amount: u64,
    other_amount_threshold: u64,
    amount_specified_is_input: bool,
    a_to_b_one: bool,
    a_to_b_two: bool,
    sqrt_price_limit_one: u128,
    sqrt_price_limit_two: u128,
    remaining_accounts_info: Option<RemainingAccountsInfo>,
) -> Result<()> {
    let clock = Clock::get()?;
    // Update the global reward growth which increases as a function of time.
    let timestamp = to_timestamp_u64(clock.unix_timestamp)?;

    let pool_one = &mut ctx.accounts.pool_one;
    let pool_two = &mut ctx.accounts.pool_two;

    // Don't allow swaps on the same pool
    if pool_one.key() == pool_two.key() {
        return Err(ErrorCode::DuplicateTwoHopPool.into());
    }

    let swap_one_output_mint = if a_to_b_one {
        pool_one.token_mint_b
    } else {
        pool_one.token_mint_a
    };

    let swap_two_input_mint = if a_to_b_two {
        pool_two.token_mint_a
    } else {
        pool_two.token_mint_b
    };
    if swap_one_output_mint != swap_two_input_mint {
        return Err(ErrorCode::InvalidIntermediaryMint.into());
    }

    // Process remaining accounts
    let remaining_accounts = parse_remaining_accounts(
        &ctx.remaining_accounts,
        &remaining_accounts_info,
        &[
            AccountsType::TransferHookInput,
            AccountsType::TransferHookIntermediate,
            AccountsType::TransferHookOutput,
        ],
    )?;

    let mut swap_tick_sequence_one = SwapTickSequence::new(
        ctx.accounts.tick_array_one_0.load_mut()?,
        ctx.accounts.tick_array_one_1.load_mut().ok(),
        ctx.accounts.tick_array_one_2.load_mut().ok(),
    );

    let mut swap_tick_sequence_two = SwapTickSequence::new(
        ctx.accounts.tick_array_two_0.load_mut()?,
        ctx.accounts.tick_array_two_1.load_mut().ok(),
        ctx.accounts.tick_array_two_2.load_mut().ok(),
    );

    // TODO: WLOG, we could extend this to N-swaps, but the account inputs to the instruction would
    // need to be jankier and we may need to programatically map/verify rather than using anchor constraints
    let (swap_update_one, swap_update_two) = if amount_specified_is_input {
        // If the amount specified is input, this means we are doing exact-in
        // and the swap calculations occur from Swap 1 => Swap 2
        // and the swaps occur from Swap 1 => Swap 2
        let swap_calc_one = swap_with_transfer_fee_extension(
            &pool_one,
            if a_to_b_one {
                &ctx.accounts.token_mint_input
            } else {
                &ctx.accounts.token_mint_intermediate
            },
            if a_to_b_one {
                &ctx.accounts.token_mint_intermediate
            } else {
                &ctx.accounts.token_mint_input
            },
            &mut swap_tick_sequence_one,
            amount,
            sqrt_price_limit_one,
            amount_specified_is_input, // true
            a_to_b_one,
            timestamp,
        )?;

        // Swap two input is the output of swap one
        // We use vault to vault transfer, so transfer fee will be collected once.
        let swap_two_input_amount = if a_to_b_one {
            swap_calc_one.amount_b
        } else {
            swap_calc_one.amount_a
        };

        let swap_calc_two = swap_with_transfer_fee_extension(
            &pool_two,
            if a_to_b_two {
                &ctx.accounts.token_mint_intermediate
            } else {
                &ctx.accounts.token_mint_output
            },
            if a_to_b_two {
                &ctx.accounts.token_mint_output
            } else {
                &ctx.accounts.token_mint_intermediate
            },
            &mut swap_tick_sequence_two,
            swap_two_input_amount,
            sqrt_price_limit_two,
            amount_specified_is_input, // true
            a_to_b_two,
            timestamp,
        )?;
        (swap_calc_one, swap_calc_two)
    } else {
        // If the amount specified is output, this means we need to invert the ordering of the calculations
        // and the swap calculations occur from Swap 2 => Swap 1
        // but the actual swaps occur from Swap 1 => Swap 2 (to ensure that the intermediate token exists in the account)
        let swap_calc_two = swap_with_transfer_fee_extension(
            &pool_two,
            if a_to_b_two {
                &ctx.accounts.token_mint_intermediate
            } else {
                &ctx.accounts.token_mint_output
            },
            if a_to_b_two {
                &ctx.accounts.token_mint_output
            } else {
                &ctx.accounts.token_mint_intermediate
            },
            &mut swap_tick_sequence_two,
            amount,
            sqrt_price_limit_two,
            amount_specified_is_input, // false
            a_to_b_two,
            timestamp,
        )?;

        // The output of swap 1 is input of swap_calc_two
        let swap_one_output_amount = if a_to_b_two {
            calculate_transfer_fee_excluded_amount(
                &ctx.accounts.token_mint_intermediate,
                swap_calc_two.amount_a,
            )?
            .amount
        } else {
            calculate_transfer_fee_excluded_amount(
                &ctx.accounts.token_mint_intermediate,
                swap_calc_two.amount_b,
            )?
            .amount
        };

        let swap_calc_one = swap_with_transfer_fee_extension(
            &pool_one,
            if a_to_b_one {
                &ctx.accounts.token_mint_input
            } else {
                &ctx.accounts.token_mint_intermediate
            },
            if a_to_b_one {
                &ctx.accounts.token_mint_intermediate
            } else {
                &ctx.accounts.token_mint_input
            },
            &mut swap_tick_sequence_one,
            swap_one_output_amount,
            sqrt_price_limit_one,
            amount_specified_is_input, // false
            a_to_b_one,
            timestamp,
        )?;
        (swap_calc_one, swap_calc_two)
    };

    // All output token should be consumed by the second swap
    let swap_calc_one_output = if a_to_b_one {
        swap_update_one.amount_b
    } else {
        swap_update_one.amount_a
    };
    let swap_calc_two_input = if a_to_b_two {
        swap_update_two.amount_a
    } else {
        swap_update_two.amount_b
    };
    if swap_calc_one_output != swap_calc_two_input {
        return Err(ErrorCode::IntermediateTokenAmountMismatch.into());
    }

    if amount_specified_is_input {
        // If amount_specified_is_input == true, then we have a variable amount of output
        // The slippage we care about is the output of the second swap.
        let output_amount = if a_to_b_two {
            calculate_transfer_fee_excluded_amount(
                &ctx.accounts.token_mint_output,
                swap_update_two.amount_b,
            )?
            .amount
        } else {
            calculate_transfer_fee_excluded_amount(
                &ctx.accounts.token_mint_output,
                swap_update_two.amount_a,
            )?
            .amount
        };

        // If we have received less than the minimum out, throw an error
        if output_amount < other_amount_threshold {
            return Err(ErrorCode::AmountOutBelowMinimum.into());
        }
    } else {
        // amount_specified_is_output == false, then we have a variable amount of input
        // The slippage we care about is the input of the first swap
        let input_amount = if a_to_b_one {
            swap_update_one.amount_a
        } else {
            swap_update_one.amount_b
        };
        if input_amount > other_amount_threshold {
            return Err(ErrorCode::AmountInAboveMaximum.into());
        }
    }

    /*
    update_and_swap_pool_v2(
        pool_one,
        &ctx.accounts.token_authority,
        &ctx.accounts.token_mint_one_a,
        &ctx.accounts.token_mint_one_b,
        &ctx.accounts.token_owner_account_one_a,
        &ctx.accounts.token_owner_account_one_b,
        &ctx.accounts.token_vault_one_a,
        &ctx.accounts.token_vault_one_b,
        &remaining_accounts.transfer_hook_one_a,
        &remaining_accounts.transfer_hook_one_b,
        &ctx.accounts.token_program_one_a,
        &ctx.accounts.token_program_one_b,
        &ctx.accounts.memo_program,
        swap_update_one,
        a_to_b_one,
        timestamp,
        transfer_memo::TRANSFER_MEMO_SWAP.as_bytes(),
    )?;

    update_and_swap_pool_v2(
        pool_two,
        &ctx.accounts.token_authority,
        &ctx.accounts.token_mint_two_a,
        &ctx.accounts.token_mint_two_b,
        &ctx.accounts.token_owner_account_two_a,
        &ctx.accounts.token_owner_account_two_b,
        &ctx.accounts.token_vault_two_a,
        &ctx.accounts.token_vault_two_b,
        &remaining_accounts.transfer_hook_two_a,
        &remaining_accounts.transfer_hook_two_b,
        &ctx.accounts.token_program_two_a,
        &ctx.accounts.token_program_two_b,
        &ctx.accounts.memo_program,
        swap_update_two,
        a_to_b_two,
        timestamp,
        transfer_memo::TRANSFER_MEMO_SWAP.as_bytes(),
    )
    */

    update_and_two_hop_swap_pool_v2(
        swap_update_one,
        swap_update_two,
        pool_one,
        pool_two,
        a_to_b_one,
        a_to_b_two,
        &ctx.accounts.token_mint_input,
        &ctx.accounts.token_mint_intermediate,
        &ctx.accounts.token_mint_output,
        &ctx.accounts.token_program_input,
        &ctx.accounts.token_program_intermediate,
        &ctx.accounts.token_program_output,
        &ctx.accounts.token_owner_account_input,
        &ctx.accounts.token_vault_one_input,
        &ctx.accounts.token_vault_one_intermediate,
        &ctx.accounts.token_vault_two_intermediate,
        &ctx.accounts.token_vault_two_output,
        &ctx.accounts.token_owner_account_output,
        &remaining_accounts.transfer_hook_input,
        &remaining_accounts.transfer_hook_intermediate,
        &remaining_accounts.transfer_hook_output,
        &ctx.accounts.token_authority,
        &ctx.accounts.memo_program,
        timestamp,
        transfer_memo::TRANSFER_MEMO_SWAP.as_bytes(),
    )
}
