//! Bitcoin payout destination for an expired customer's mailbox key.

use bitcoin::secp256k1::PublicKey;
use bitcoin::ScriptBuf;

use crate::SECP;

/// The Taproot output that expired coins of mailbox `M` are paid to: `M`
/// lifted to even Y, tweaked with `TapTweak` and an empty script tree.
pub fn payout_script(mailbox: PublicKey) -> ScriptBuf {
	ScriptBuf::new_p2tr(&SECP, mailbox.x_only_public_key().0, None)
}

#[cfg(test)]
mod test {
	use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
	use bitcoin::hashes::Hash;
	use bitcoin::secp256k1::{schnorr, Keypair, Message};
	use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
	use bitcoin_ext::KeypairExt;

	use crate::{ProtocolEncoding, SECP};
	use crate::mailbox::MailboxAuthorization;

	/// The payout key `M` also signs mailbox authorizations. Neither kind of
	/// signature can stand in for the other.
	#[test]
	fn mailbox_authorization_cannot_authorize_a_taproot_input() {
		let mut secret = [0; 32];
		secret[31] = 6;
		let mailbox = Keypair::from_seckey_slice(&SECP, &secret).unwrap();
		assert_eq!(mailbox.public_key().serialize()[0], 3, "exercise an odd full mailbox point");
		let authorization = MailboxAuthorization::new(&mailbox, chrono::Local::now() + chrono::Duration::hours(1));
		assert!(authorization.verify());

		// The signature is the last field of the encoding.
		let encoded = authorization.serialize();
		let sig_at = encoded.len() - 64;
		let auth_sig = schnorr::Signature::from_slice(&encoded[sig_at..]).unwrap();
		let with_sig = |sig: schnorr::Signature| {
			let mut bytes = encoded.clone();
			bytes[sig_at..].copy_from_slice(sig.as_ref());
			MailboxAuthorization::deserialize(&bytes).unwrap()
		};

		let prevout = TxOut { value: Amount::from_sat(10_000),
			script_pubkey: ScriptBuf::new_p2tr(&SECP, mailbox.x_only_public_key().0, None) };
		let mut spend = Transaction {
			version: bitcoin::transaction::Version::TWO, lock_time: bitcoin::absolute::LockTime::ZERO,
			input: vec![TxIn { previous_output: OutPoint::new(Txid::from_byte_array([1; 32]), 0),
				script_sig: ScriptBuf::new(), sequence: Sequence::MAX, witness: Witness::new() }],
			output: vec![TxOut { value: Amount::from_sat(9_000), script_pubkey: prevout.script_pubkey.clone() }],
		};
		let sighash: Message = SighashCache::new(&spend).taproot_key_spend_signature_hash(
			0, &Prevouts::All(&[prevout]), TapSighashType::Default,
		).unwrap().into();
		let signing_key = mailbox.for_keyspend_only(&SECP);
		let signature = SECP.sign_schnorr_no_aux_rand(&sighash, &signing_key);
		spend.input[0].witness = Witness::from_slice(&[&signature[..]]);
		assert_eq!(spend.input[0].witness.len(), 1);
		SECP.verify_schnorr(&signature, &sighash, &signing_key.x_only_public_key().0).unwrap();
		assert!(SECP.verify_schnorr(&auth_sig, &sighash, &signing_key.x_only_public_key().0).is_err());
		assert!(SECP.verify_schnorr(&auth_sig, &sighash, &mailbox.x_only_public_key().0).is_err(),
			"the message domains differ even before the Taproot key tweak");
		assert!(!with_sig(signature).verify());
		let raw_input_signature = SECP.sign_schnorr_no_aux_rand(&sighash, &mailbox);
		assert!(!with_sig(raw_input_signature).verify());
	}
}
