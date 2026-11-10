//! Bitcoin payout destination for an expired customer's mailbox key.

use bitcoin::secp256k1::PublicKey;
use bitcoin::ScriptBuf;

use crate::SECP;

/// The Taproot output that expired coins of mailbox `M` are paid to: `M`
/// lifted to even Y, tweaked with `TapTweak` and an empty script tree.
pub fn payout_script(mailbox: PublicKey) -> ScriptBuf {
	ScriptBuf::new_p2tr(&SECP, mailbox.x_only_public_key().0, None)
}
