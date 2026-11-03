//! Recipient amounts for a direct expiry claim. Ownership is resolved before this step.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context};
use bitcoin::{Amount, ScriptBuf};

use ark::VtxoId;

/// Fee for all payouts to one script at one expiry height.
fn payout_fee(gross: Amount) -> Amount {
	Amount::from_sat(1_000.max(gross.to_sat().div_ceil(100)))
}

/// A customer entitlement whose ownership and backing have been resolved.
pub(crate) struct CustomerLeaf {
	pub id: VtxoId,
	pub script: ScriptBuf,
	pub amount: Amount,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LeafAllocation {
	pub id: VtxoId,
	pub gross: Amount,
	pub fee: Amount,
	pub payout: Amount,
	pub forfeited: Amount,
}

/// Group by output, with stable per-leaf amounts for persisted attribution.
/// Mining cost is independent: the watchman wallet funds the fee child,
/// and the sweep remainder returns to the operator.
pub(crate) fn allocate(
	leaves: Vec<CustomerLeaf>, already_paid: &BTreeMap<ScriptBuf, (Amount, Amount)>,
) -> anyhow::Result<Vec<LeafAllocation>> {
	let mut ids = BTreeSet::new();
	let mut groups = BTreeMap::<ScriptBuf, Vec<CustomerLeaf>>::new();
	for leaf in leaves {
		ensure!(ids.insert(leaf.id), "duplicate expiry entitlement {}", leaf.id);
		ensure!(leaf.amount > Amount::ZERO, "empty expiry entitlement {}", leaf.id);
		groups.entry(leaf.script.clone()).or_default().push(leaf);
	}

	let mut allocations = Vec::new();
	for (script, mut group) in groups {
		group.sort_by_key(|leaf| leaf.id);
		let gross = group.iter().try_fold(Amount::ZERO, |sum, leaf|
			sum.checked_add(leaf.amount).context("expiry group amount overflow"),
		)?;
		let (paid_gross, paid_fee) = already_paid.get(&script).copied().unwrap_or_default();
		let total = paid_gross.checked_add(gross).context("expiry paid principal overflow")?;
		let group_fee = payout_fee(total).checked_sub(paid_fee)
			.context("expiry fee history exceeds the total fee")?.min(gross);
		let net = gross.checked_sub(group_fee).context("expiry fee exceeds principal")?;
		let pays = net > Amount::ZERO && net >= script.minimal_non_dust();
		// There is no output fee when the entire group is forfeited.
		let mut remaining_fee = if pays { group_fee } else { Amount::ZERO };
		for leaf in group {
			let fee = leaf.amount.min(remaining_fee);
			remaining_fee = remaining_fee.checked_sub(fee).context("expiry fee allocation overflow")?;
			let remainder = leaf.amount.checked_sub(fee).context("expiry leaf fee exceeds principal")?;
			allocations.push(LeafAllocation {
				id: leaf.id, gross: leaf.amount, fee,
				payout: if pays { remainder } else { Amount::ZERO },
				forfeited: if pays { Amount::ZERO } else { remainder },
			});
		}
		ensure!(remaining_fee == Amount::ZERO, "expiry group fee was not allocated");
	}
	Ok(allocations)
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{PubkeyHash, ScriptHash, WPubkeyHash, WScriptHash};
	use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};

	use super::*;

	fn allocate_first(leaves: Vec<CustomerLeaf>) -> anyhow::Result<Vec<LeafAllocation>> {
		allocate(leaves, &BTreeMap::new())
	}

	fn leaf(id: u8, amount: u64, script: &ScriptBuf) -> CustomerLeaf {
		let mut bytes = [0; VtxoId::ENCODE_SIZE];
		bytes[0] = id;
		CustomerLeaf {
			id: VtxoId::from_slice(&bytes).unwrap(),
			amount: Amount::from_sat(amount),
			script: script.clone()
		}
	}

	fn wpkh() -> ScriptBuf {
		ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([1; 20]))
	}

	#[test]
	fn direct_fee_many_small_leaves_pay_one_non_dust_output() {
		let script = wpkh();
		let plan = allocate_first((0..20).map(|i| leaf(i, 100, &script)).collect()).unwrap();
		assert!(plan.iter().all(|leaf| leaf.forfeited == Amount::ZERO));
		assert_eq!(plan.len(), 20);
		let recipient = &plan;
		assert_eq!(recipient.iter().map(|leaf| leaf.payout).sum::<Amount>(), Amount::from_sat(1_000));
		assert_eq!(recipient.iter().map(|a| a.fee.to_sat()).sum::<u64>(), 1_000);
		assert!(recipient.iter().all(|a| a.fee <= a.gross));
	}

	#[test]
	fn direct_fee_script_dust_equality_is_paid() {
		let secp = Secp256k1::new();
		let key = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
		for script in [
			wpkh(),
			ScriptBuf::new_p2pkh(&PubkeyHash::from_byte_array([2; 20])),
			ScriptBuf::new_p2sh(&ScriptHash::from_byte_array([4; 20])),
			ScriptBuf::new_p2wsh(&WScriptHash::from_byte_array([5; 32])),
			ScriptBuf::new_p2tr(&secp, key.x_only_public_key().0, None),
		] {
			let dust = script.minimal_non_dust().to_sat();
			let paid = allocate_first(vec![leaf(1, 1_000 + dust, &script)]).unwrap();
			assert_eq!(paid.iter().map(|leaf| leaf.payout).sum::<Amount>().to_sat(), dust);
			let forfeited = allocate_first(vec![leaf(1, 999 + dust, &script)]).unwrap();
			assert!(forfeited.iter().all(|leaf| leaf.payout == Amount::ZERO));
			assert_eq!(forfeited[0].forfeited.to_sat(), 999 + dust);
			assert_eq!(forfeited[0].fee, Amount::ZERO);
		}
	}

	#[test]
	fn direct_fee_order_does_not_change_leaf_attribution() {
		let script = wpkh();
		let a = allocate_first(vec![leaf(2, 2_000, &script), leaf(1, 400, &script)]).unwrap();
		let b = allocate_first(vec![leaf(1, 400, &script), leaf(2, 2_000, &script)]).unwrap();
		assert_eq!(a, b);
		assert_eq!(a[0].fee.to_sat(), 400);
		assert_eq!(a[1].fee.to_sat(), 600);
	}

	#[test]
	fn direct_fee_rejects_duplicate_entitlement() {
		let script = wpkh();
		assert!(allocate_first(vec![leaf(1, 2_000, &script), leaf(1, 2_000, &script)]).is_err());
	}

	#[test]
	fn direct_fee_conserves_each_leaf_across_policy_boundary() {
		let script = wpkh();
		for total in 1..4_000 {
			let plan = allocate_first(vec![leaf(1, total, &script)]).unwrap();
			let allocations = plan.iter();
			for a in allocations {
				assert_eq!(a.gross.to_sat(), a.fee.to_sat() + a.payout.to_sat() + a.forfeited.to_sat());
				assert!(a.payout == Amount::ZERO || a.forfeited == Amount::ZERO);
			}
		}
	}

	#[test]
	fn direct_fee_separate_outputs_forfeit_what_a_merged_output_pays() {
		let script = wpkh();
		let combined = allocate_first(vec![leaf(1, 1_200, &script), leaf(2, 1_200, &script)]).unwrap();
		assert_eq!(combined.len(), 2);
		assert_eq!(combined.iter().map(|leaf| leaf.payout).sum::<Amount>().to_sat(), 1_400);
		for id in [1, 2] {
			let separate = allocate_first(vec![leaf(id, 1_200, &script)]).unwrap();
			assert!(separate.iter().all(|leaf| leaf.payout == Amount::ZERO));
			assert_eq!(separate[0].forfeited.to_sat(), 1_200);
		}
	}

	#[test]
	fn fee_is_shared_across_allocations_at_one_height() {
		let script = wpkh();
		let first = allocate_first(vec![leaf(1, 50_000, &script)]).unwrap();
		assert_eq!(first.iter().map(|leaf| leaf.payout).sum::<Amount>().to_sat(), 49_000);
		let paid = BTreeMap::from([(script.clone(), (Amount::from_sat(50_000), Amount::from_sat(1_000)))]);
		let second = allocate(vec![leaf(2, 50_001, &script)], &paid).unwrap();
		assert_eq!(second.iter().map(|leaf| leaf.payout).sum::<Amount>().to_sat(), 50_000);
		assert_eq!(second[0].fee.to_sat(), 1);
		assert_eq!(49_000 + 50_000, 100_001 - 1_001);
		let different = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([9; 20]));
		let other = allocate(vec![leaf(3, 50_000, &different)], &paid).unwrap();
		assert_eq!(other.iter().map(|leaf| leaf.payout).sum::<Amount>().to_sat(), 49_000);
	}

	#[test]
	fn percentage_fee_rounds_up_without_overflow() {
		for (gross, fee) in [(100_000, 1_000), (100_001, 1_001), (200_000, 2_000),
			(u64::MAX, u64::MAX.div_ceil(100))] {
			assert_eq!(payout_fee(Amount::from_sat(gross)).to_sat(), fee);
		}
	}

}
