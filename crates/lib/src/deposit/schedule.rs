//! Chain-derived send scheduling — the multi-instance safety core.
//!
//! "Does deposit X still need a burn?" and "does this lock unit still need a
//! lock?" are never answered from the local store (a possibly-stale,
//! LWW-synced cache): they are re-derived from chain logs, which every
//! instance reads identically. Burns/locks are matched to their causes
//! greedily in chain order **by amount** — inflow amounts are fungible
//! within the one deposit address, so when two pending items share an
//! amount, which physical log gets which label is economically irrelevant;
//! what matters is that the COUNT of sends per amount can never exceed the
//! count of causes. Any log that matches nothing is foreign or ahead of our
//! view: the schedule goes [`Inconsistent`](ScheduleError::Inconsistent) and
//! the caller stalls (re-scan and retry) instead of sending — a liveness
//! pause, never a double-spend.

use crate::deposit::models::{Deposit, DepositStatus, DepositSwap};

/// A `DepositForBurn` observed on a source chain (from our address).
#[derive(Clone, Debug)]
pub(crate) struct ObservedBurn {
    pub amount: u64,
    pub tx_hash: String,
}

/// A commitment `Lockup` observed on Arbitrum (refundAddress = us,
/// preimageHash = zero).
#[derive(Clone, Debug)]
pub(crate) struct ObservedLock {
    pub amount: u64,
    pub tx_hash: String,
    pub log_index: u64,
    pub timelock: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleError {
    /// An observed send matches no known cause — our view is behind (an
    /// unscanned inflow was already acted on) or something foreign spent
    /// from the deposit address. Stall, re-scan, retry.
    Inconsistent(String),
}

/// Outcome of matching observed burns against a chain's inflows.
#[derive(Debug)]
pub(crate) struct BurnSchedule {
    /// `(deposit_id, burn_tx_hash)` — inflows whose burn already exists
    /// on-chain (whoever sent it). The engine adopts these instead of
    /// re-burning; a parked inflow appearing here was burned by an instance
    /// with a different park verdict and must be un-parked and adopted too.
    pub adopted: Vec<(String, String)>,
    /// The single inflow eligible to burn next (first unmatched, in chain
    /// order, skipping parked). Strictly one at a time: burns are
    /// nonce-serialized anyway, and one-in-flight keeps amount-matching
    /// unambiguous.
    pub next: Option<String>,
    /// Sum of every unmatched (un-burned) inflow's amount — the conservation
    /// floor: the address balance must cover it before `next` may burn.
    pub unburned_total: u64,
}

/// Match observed burns against this chain's inflows (any status, chain
/// order). `deposits` MUST be every recorded inflow for the chain — filtering
/// first breaks the greedy alignment.
pub(crate) fn derive_burn_schedule(
    deposits: &[Deposit],
    burns: &[ObservedBurn],
) -> Result<BurnSchedule, ScheduleError> {
    let ordered = chain_ordered(deposits);

    let mut matched: Vec<Option<&ObservedBurn>> = vec![None; ordered.len()];
    for burn in burns {
        let slot = ordered.iter().enumerate().find(|(i, d)| {
            matched[*i].is_none()
                && d.amount == burn.amount
                && d.chain_id != crate::config::ARBITRUM_CHAIN_ID
        });
        match slot {
            Some((i, _)) => matched[i] = Some(burn),
            None => {
                return Err(ScheduleError::Inconsistent(format!(
                    "burn {} (amount {}) matches no recorded inflow — view behind or foreign spend",
                    burn.tx_hash, burn.amount
                )));
            }
        }
    }

    let mut adopted = Vec::new();
    let mut next = None;
    let mut unburned_total: u64 = 0;
    for (i, deposit) in ordered.iter().enumerate() {
        if deposit.chain_id == crate::config::ARBITRUM_CHAIN_ID {
            continue; // local inflows never burn
        }
        if let Some(burn) = matched[i] {
            adopted.push((deposit.id.clone(), burn.tx_hash.clone()));
            continue;
        }
        unburned_total = unburned_total.saturating_add(deposit.amount);
        let parked = matches!(deposit.status, DepositStatus::Parked { .. });
        let consumed = matches!(deposit.status, DepositStatus::Consumed);
        if next.is_none() && !parked && !consumed {
            next = Some(deposit.id.clone());
        }
    }

    Ok(BurnSchedule {
        adopted,
        next,
        unburned_total,
    })
}

/// Outcome of matching observed commitment locks against active lock units.
#[derive(Debug)]
pub(crate) struct LockSchedule {
    /// `(deposit_swap_id, lock)` — units whose lock already exists on-chain.
    pub adopted: Vec<(String, ObservedLock)>,
    /// The single unit eligible to lock next (first unmatched, creation
    /// order). One at a time, same rationale as burns.
    pub next: Option<String>,
}

/// Match observed commitment `Lockup`s against active lock units, in unit
/// creation order. `swaps` MUST be every non-terminal unit; `locks` every
/// commitment Lockup with `refundAddress` = us since the earliest unit's
/// `created_at_block`.
pub(crate) fn derive_lock_schedule(
    swaps: &[DepositSwap],
    locks: &[ObservedLock],
) -> Result<LockSchedule, ScheduleError> {
    let mut ordered: Vec<&DepositSwap> = swaps.iter().collect();
    ordered.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));

    let mut matched: Vec<Option<&ObservedLock>> = vec![None; ordered.len()];
    for lock in locks {
        // A unit that already recorded its commitment tx claims its lock by
        // identity; otherwise greedy amount matching.
        let slot = ordered
            .iter()
            .enumerate()
            .find(|(i, s)| {
                matched[*i].is_none()
                    && s.commitment_tx_hash
                        .as_deref()
                        .is_some_and(|h| h.eq_ignore_ascii_case(&lock.tx_hash))
            })
            .or_else(|| {
                ordered.iter().enumerate().find(|(i, s)| {
                    matched[*i].is_none()
                        && s.commitment_tx_hash.is_none()
                        && s.amount == lock.amount
                })
            });
        match slot {
            Some((i, _)) => matched[i] = Some(lock),
            None => {
                return Err(ScheduleError::Inconsistent(format!(
                    "commitment lock {}:{} (amount {}) matches no active lock unit",
                    lock.tx_hash, lock.log_index, lock.amount
                )));
            }
        }
    }

    let mut adopted = Vec::new();
    let mut next = None;
    for (i, swap) in ordered.iter().enumerate() {
        if let Some(lock) = matched[i] {
            adopted.push((swap.id.clone(), lock.clone()));
        } else if next.is_none() && swap.commitment_tx_hash.is_none() {
            next = Some(swap.id.clone());
        }
    }

    Ok(LockSchedule { adopted, next })
}

/// Canonical chain order for inflows: (block, txHash, logIndex).
fn chain_ordered(deposits: &[Deposit]) -> Vec<&Deposit> {
    let mut ordered: Vec<&Deposit> = deposits.iter().collect();
    ordered.sort_by(|a, b| {
        (a.block_number, &a.tx_hash, a.log_index).cmp(&(b.block_number, &b.tx_hash, b.log_index))
    });
    ordered
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::deposit::models::{DepositSwapStatus, ParkReason};

    fn dep(id: &str, block: u64, amount: u64, status: DepositStatus) -> Deposit {
        Deposit {
            id: id.to_string(),
            status,
            chain_id: 8453,
            tx_hash: format!("0x{id}"),
            log_index: 0,
            block_number: block,
            amount,
            deposit_address: "0xd".to_string(),
            pending_send: None,
            burn_tx_hash: None,
            cctp_nonce: None,
            mint_deadline: None,
            minted_amount: None,
            deposit_swap_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn burn(tx: &str, amount: u64) -> ObservedBurn {
        ObservedBurn {
            amount,
            tx_hash: tx.to_string(),
        }
    }

    #[macros::test_all]
    fn empty_chain_yields_nothing() {
        let s = derive_burn_schedule(&[], &[]).unwrap();
        assert!(s.adopted.is_empty());
        assert!(s.next.is_none());
        assert_eq!(s.unburned_total, 0);
    }

    #[macros::test_all]
    fn fifo_matching_adopts_and_picks_next() {
        let deposits = vec![
            dep("a", 10, 100, DepositStatus::Bridging), // burned by us earlier
            dep("b", 20, 200, DepositStatus::Detected),
            dep("c", 30, 300, DepositStatus::Detected),
        ];
        let burns = vec![burn("0xburn-a", 100)];

        let s = derive_burn_schedule(&deposits, &burns).unwrap();
        assert_eq!(s.adopted, vec![("a".to_string(), "0xburn-a".to_string())]);
        assert_eq!(s.next.as_deref(), Some("b"));
        assert_eq!(s.unburned_total, 500);
    }

    #[macros::test_all]
    fn parked_inflow_is_skipped_for_next_but_still_matchable() {
        // Instance B (different fee view) burned "a" even though WE parked it:
        // the burn must match and adopt, not go inconsistent.
        let deposits = vec![
            dep(
                "a",
                10,
                100,
                DepositStatus::Parked {
                    reason: ParkReason::BelowBridgeFee,
                },
            ),
            dep("b", 20, 200, DepositStatus::Detected),
        ];

        // No burns: parked "a" is not `next` (b is), but counts as unburned.
        let s = derive_burn_schedule(&deposits, &[]).unwrap();
        assert_eq!(s.next.as_deref(), Some("b"));
        assert_eq!(s.unburned_total, 300);

        // B's burn of the parked inflow adopts.
        let s = derive_burn_schedule(&deposits, &[burn("0xother", 100)]).unwrap();
        assert_eq!(s.adopted, vec![("a".to_string(), "0xother".to_string())]);
        assert_eq!(s.next.as_deref(), Some("b"));
        assert_eq!(s.unburned_total, 200);
    }

    #[macros::test_all]
    fn equal_amounts_match_fungibly() {
        let deposits = vec![
            dep("a", 10, 100, DepositStatus::Detected),
            dep("b", 20, 100, DepositStatus::Detected),
        ];
        let s = derive_burn_schedule(&deposits, &[burn("0xone", 100)]).unwrap();
        // Which of the two got labeled is irrelevant; exactly one did.
        assert_eq!(s.adopted.len(), 1);
        assert_eq!(s.adopted[0].0, "a"); // greedy = chain order
        assert_eq!(s.next.as_deref(), Some("b"));
    }

    #[macros::test_all]
    fn unmatched_burn_is_inconsistent() {
        let deposits = vec![dep("a", 10, 100, DepositStatus::Detected)];
        // Amount matches nothing we know -> behind view / foreign spend.
        let err = derive_burn_schedule(&deposits, &[burn("0xghost", 999)]).unwrap_err();
        assert!(matches!(err, ScheduleError::Inconsistent(_)));

        // More burns than inflows -> also inconsistent.
        let err =
            derive_burn_schedule(&deposits, &[burn("0x1", 100), burn("0x2", 100)]).unwrap_err();
        assert!(matches!(err, ScheduleError::Inconsistent(_)));
    }

    fn unit(id_seed: &str, created_at: u64, amount: u64) -> DepositSwap {
        let ids = vec![format!("8453:0x{id_seed}:0")];
        DepositSwap {
            id: DepositSwap::derive_id(&ids),
            status: DepositSwapStatus::Locking,
            deposit_ids: ids,
            amount,
            deposit_address: "0xd".to_string(),
            created_at_block: 1,
            erc20swap_address: None,
            claim_address: None,
            timelock: None,
            pending_send: None,
            commitment_tx_hash: None,
            commitment_log_index: None,
            invoice: None,
            invoice_amount_sats: None,
            swap_id: None,
            expected_amount: None,
            bound: false,
            refund_tx_hash: None,
            created_at,
            updated_at: created_at,
        }
    }

    fn lock(tx: &str, amount: u64) -> ObservedLock {
        ObservedLock {
            amount,
            tx_hash: tx.to_string(),
            log_index: 0,
            timelock: 25_000_000,
        }
    }

    #[macros::test_all]
    fn lock_matching_prefers_identity_then_amount() {
        let mut with_tx = unit("a", 100, 500);
        with_tx.commitment_tx_hash = Some("0xLOCK1".to_string());
        let unlocked = unit("b", 200, 500); // same amount!

        let locks = vec![lock("0xlock1", 500), lock("0xlock2", 500)];
        let s = derive_lock_schedule(&[with_tx.clone(), unlocked.clone()], &locks).unwrap();

        // 0xlock1 claims the identity match even though "b" also fits by
        // amount; 0xlock2 adopts into "b".
        let adopted_ids: Vec<&str> = s.adopted.iter().map(|(id, _)| id.as_str()).collect();
        assert!(adopted_ids.contains(&with_tx.id.as_str()));
        assert!(adopted_ids.contains(&unlocked.id.as_str()));
        let b_lock = s.adopted.iter().find(|(id, _)| id == &unlocked.id).unwrap();
        assert_eq!(b_lock.1.tx_hash, "0xlock2");
        assert_eq!(b_lock.1.timelock, 25_000_000);
        assert!(s.next.is_none());
    }

    #[macros::test_all]
    fn lock_schedule_next_and_inconsistency() {
        let a = unit("a", 100, 500);
        let s = derive_lock_schedule(std::slice::from_ref(&a), &[]).unwrap();
        assert_eq!(s.next.as_deref(), Some(a.id.as_str()));

        let err = derive_lock_schedule(&[a], &[lock("0xghost", 777)]).unwrap_err();
        assert!(matches!(err, ScheduleError::Inconsistent(_)));
    }
}
