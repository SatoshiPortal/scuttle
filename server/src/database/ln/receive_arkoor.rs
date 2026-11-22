//! The arkoor that settles a lightning receive paid on the Ark address in
//! the route hints of its invoice.

use anyhow::Context;

use ark::VtxoId;
use ark::mailbox::MailboxIdentifier;

use crate::database::Tx;

use super::LightningHtlcSubscriptionStatus;

/// The VTXO ids of `inputs`, sorted, as the receive stores them.
fn sorted_ids(inputs: &[VtxoId]) -> Vec<String> {
	let mut ids = inputs.iter().map(ToString::to_string).collect::<Vec<_>>();
	ids.sort_unstable();
	ids
}

impl Tx<'_> {
	/// Settle the open receive `id` by a new arkoor that spends `inputs`,
	/// and record those inputs. An arkoor that the server co-signed before,
	/// through any call, has already paid where it pays, so it cannot settle
	/// a receive.
	pub(crate) async fn settle_lightning_receive_by_arkoor(
		&self,
		id: i64,
		inputs: &[VtxoId],
	) -> anyhow::Result<()> {
		let ids = sorted_ids(inputs);
		let spent = self.query_opt(
			"SELECT vtxo_id FROM vtxo WHERE vtxo_id = ANY($1) AND oor_spent_txid IS NOT NULL LIMIT 1",
			&[&ids],
		).await?;
		if spent.is_some() {
			return badarg!("arkoor was co-signed before and cannot settle the invoice");
		}
		let settled = self.execute("
			UPDATE lightning_htlc_subscription
			SET updated_at = NOW(), status = $2, arkoor_input_ids = $3
			WHERE id = $1 AND status = $4
		", &[
			&id, &LightningHtlcSubscriptionStatus::Settled, &ids, &LightningHtlcSubscriptionStatus::Created,
		]).await?;
		if settled != 1 {
			return badarg!("invoice is not payable");
		}
		Ok(())
	}

	/// Whether the arkoor that spends `inputs` settled the receive `id`.
	pub(crate) async fn lightning_receive_settled_by_arkoor(
		&self,
		id: i64,
		inputs: &[VtxoId],
	) -> anyhow::Result<bool> {
		let row = self.query_opt("
			SELECT id FROM lightning_htlc_subscription
			WHERE id = $1 AND status = $2 AND arkoor_input_ids = $3
		", &[&id, &LightningHtlcSubscriptionStatus::Settled, &sorted_ids(inputs)]).await?;
		Ok(row.is_some())
	}

	/// Record the `outputs` of the arkoor that settles the receive `id` and
	/// that pay the Ark address of its invoice, whose mailbox is `mailbox`.
	/// Only a server in expiry payout mode records them.
	pub async fn record_arkoor_receive_outputs(
		&self,
		id: i64,
		mailbox: MailboxIdentifier,
		outputs: &[VtxoId],
	) -> anyhow::Result<()> {
		let ids = outputs.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.execute("
			INSERT INTO arkoor_receive_output (vtxo_id, subscription_id, mailbox_pk)
			SELECT UNNEST($1::TEXT[]), $2, $3
		", &[&ids, &id, &mailbox.to_string()]).await?;
		Ok(())
	}

	/// Mark each recorded arkoor receive output in `ids` delivered when the
	/// server holds its signed transactions and the identical VTXO was
	/// posted to the mailbox of the receive, while it is unspent, unexpired,
	/// not exited and has no expiry payout.
	///
	/// The registration and the mailbox post of an output can come in either
	/// order, in concurrent transactions, and each one calls this. The VTXOs
	/// are locked in the order of `guard_spend_changes`, so the second call
	/// waits for the first one to commit, and its next statement sees both
	/// writes.
	pub async fn certify_arkoor_receive_outputs(&self, ids: &[VtxoId]) -> anyhow::Result<()> {
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		let pending = self.query("
			SELECT v.vtxo_id FROM arkoor_receive_output r JOIN vtxo v ON v.vtxo_id = r.vtxo_id
			WHERE r.vtxo_id = ANY($1) AND r.delivered_height IS NULL
			ORDER BY v.vtxo_id FOR NO KEY UPDATE OF v
		", &[&ids]).await?;
		if pending.is_empty() {
			return Ok(());
		}

		// Without a tip the expiry cannot be checked, so the write fails and
		// the client sends it again.
		let tip = self.query_opt("SELECT height FROM captaind_block ORDER BY height DESC LIMIT 1", &[])
			.await?.context("no chain tip to check arkoor receive outputs against")?
			.get::<_, i64>(0);
		self.execute("
			UPDATE arkoor_receive_output r SET delivered_height = $2::BIGINT
			FROM vtxo v, mailbox m, vtxo_owner_mailbox o
			WHERE r.vtxo_id = ANY($1) AND r.delivered_height IS NULL
				AND v.vtxo_id = r.vtxo_id AND m.vtxo_id = r.vtxo_id AND o.vtxo_id = r.vtxo_id
				AND m.mailbox_type = 'arkoor-receive' AND m.unblinded_mailbox_id = r.mailbox_pk
				AND o.mailbox_pk = r.mailbox_pk AND m.vtxo = v.vtxo
				AND v.spend_state = 'spendable' AND v.confirmed_height IS NULL AND v.expiry > $2::BIGINT
				AND NOT EXISTS (SELECT 1 FROM expiry_payout_vtxo p WHERE p.vtxo_id = r.vtxo_id)
		", &[&ids, &tip]).await?;
		Ok(())
	}

	/// Whether the arkoor that settled the receive `id` reached the mailbox
	/// of the Ark address of its invoice: every output recorded for it at
	/// co-sign is delivered. `None` if the server recorded no output for it.
	pub async fn arkoor_receive_delivered(&self, id: i64) -> anyhow::Result<Option<bool>> {
		let row = self.query_one("
			SELECT COUNT(vtxo_id), COUNT(delivered_height)
			FROM arkoor_receive_output WHERE subscription_id = $1
		", &[&id]).await?;
		let (outputs, delivered) = (row.get::<_, i64>(0), row.get::<_, i64>(1));
		Ok((outputs > 0).then_some(outputs == delivered))
	}
}
