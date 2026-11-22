//! The delivery states of the outputs of an arkoor that pays a lightning
//! receive, see `Tx::certify_arkoor_receive_outputs`.

use std::collections::BTreeSet;

use bitcoin::{OutPoint, Txid};
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

use ark::VtxoId;
use ark::mailbox::MailboxIdentifier;
use server::database::{Db, Tx};

use ark_testing::TestContext;

/// The stored and posted bytes of every test VTXO.
const SIGNED: &[u8] = &[1];

fn vtxo_id(n: u8) -> VtxoId {
	OutPoint::new(Txid::from_byte_array([n; 32]), 0).into()
}

fn pubkey(n: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}

/// Insert a VTXO in `state` that expires at height `expiry`.
async fn insert_vtxo(t: &Tx<'_>, id: VtxoId, state: &str, expiry: i32) -> anyhow::Result<()> {
	t.execute("INSERT INTO vtxo (vtxo_id, vtxo_txid, vtxo, expiry, exit_delta, policy_type, policy,
			server_pubkey, amount, anchor_point, spend_state, created_at, updated_at)
		VALUES ($1, 'txid', $2, $3, 10, 'pubkey', '\\x00'::BYTEA, $4, 10000, 'anchor',
			$5::TEXT::spend_state, NOW(), NOW())",
		&[&id.to_string(), &SIGNED, &expiry, &pubkey(9).to_string(), &state]).await?;
	Ok(())
}

/// Post `id` with `bytes` to the arkoor mailbox `mailbox`.
async fn post(t: &Tx<'_>, id: VtxoId, mailbox: PublicKey, bytes: &[u8]) -> anyhow::Result<()> {
	t.execute("INSERT INTO mailbox (unblinded_mailbox_id, vtxo_id, vtxo, checkpoint, created_at, mailbox_type)
		VALUES ($1, $2, $3, 1, NOW(), 'arkoor-receive')",
		&[&mailbox.to_string(), &id.to_string(), &bytes]).await?;
	Ok(())
}

/// The recorded outputs that are delivered.
async fn delivered(db: &Db) -> BTreeSet<String> {
	db.read(async |t| Ok(t.query(
		"SELECT vtxo_id FROM arkoor_receive_output WHERE delivered_height IS NOT NULL", &[],
	).await?.into_iter().map(|row| row.get::<_, String>(0)).collect())).await.unwrap()
}

/// Run `certify_arkoor_receive_outputs` for `ids` in its own transaction.
async fn certify(db: &Db, ids: &[VtxoId]) -> anyhow::Result<()> {
	db.write(async |t| t.certify_arkoor_receive_outputs(ids).await).await
}

/// Whether the arkoor of each receive is delivered.
async fn receives_delivered(db: &Db, receives: &[i64]) -> Vec<Option<bool>> {
	let mut ret = Vec::new();
	for id in receives {
		ret.push(db.read(async |t| t.arkoor_receive_delivered(*id).await).await.unwrap());
	}
	ret
}

fn ids(ids: &[VtxoId]) -> BTreeSet<String> {
	ids.iter().map(ToString::to_string).collect()
}

/// An output is delivered once the server holds its signed VTXO and the
/// identical VTXO is posted to the recorded mailbox, while it is unspent,
/// unexpired, not exited and has no expiry payout. A receive is delivered
/// once all its outputs are. A delivered output stays delivered.
#[tokio::test]
async fn arkoor_receive_delivery_states() {
	let mut ctx = TestContext::new_minimal("postgresd/arkoor_receive_delivery_states").await;
	ctx.init_central_postgres().await;
	let config = ctx.new_postgres(&ctx.test_name).await;
	let db = Db::create(&config).await.unwrap();

	let mailbox = MailboxIdentifier::from_pubkey(pubkey(1));
	let other_mailbox = pubkey(2);
	// The first receive is paid by a and b, the second by c and the third
	// by d, which is bound to another mailbox. The fourth is paid by e,
	// which is expired, f, which is exited, and g, which has a payout.
	let [a, b, c, d, e, f, g, unrelated] = [1, 2, 3, 4, 5, 6, 7, 8].map(vtxo_id);
	let receives = db.write(async |t| {
		let node = t.query_one("INSERT INTO lightning_node
			(pubkey, payment_created_index, payment_updated_index, created_at, updated_at)
			VALUES ('\\x02'::BYTEA, 0, 0, NOW(), NOW()) RETURNING id", &[]).await?.get::<_, i64>(0);
		let mut receives = Vec::new();
		for n in 1..=4 {
			receives.push(t.query_one("INSERT INTO lightning_htlc_subscription
				(lightning_node_id, status, created_at, updated_at, payment_hash, invoice, arkoor_input_ids)
				VALUES ($1, 'settled', NOW(), NOW(), $2, $3, ARRAY['input']) RETURNING id",
				&[&node, &format!("receive-{n}"), &format!("invoice-{n}")]).await?.get::<_, i64>(0));
		}

		for (id, state, expiry) in [
			(a, "spendable", 200), (b, "unregistered", 200), (c, "spendable", 200), (d, "spendable", 200),
			(e, "spendable", 100), (f, "spendable", 200), (g, "spendable", 200), (unrelated, "spendable", 200),
		] {
			insert_vtxo(t, id, state, expiry).await?;
			let owner = if id == d { other_mailbox } else { mailbox.as_pubkey() };
			t.execute("INSERT INTO vtxo_owner_mailbox (vtxo_id, mailbox_pk) VALUES ($1, $2)",
				&[&id.to_string(), &owner.to_string()]).await?;
		}
		t.execute("UPDATE vtxo SET confirmed_height = 90 WHERE vtxo_id = $1", &[&f.to_string()]).await?;
		let payout = t.query_one("INSERT INTO expiry_payout (expiry_height) VALUES (200) RETURNING id", &[])
			.await?.get::<_, i64>(0);
		t.execute("INSERT INTO expiry_payout_vtxo (expiry_payout_id, vtxo_id, disposition,
			gross_sat, fee_sat, payout_sat, replannable) VALUES ($1, $2, 'operator', 10000, 0, 0, FALSE)",
			&[&payout, &g.to_string()]).await?;

		t.record_arkoor_receive_outputs(receives[0], mailbox, &[a, b]).await?;
		t.record_arkoor_receive_outputs(receives[1], mailbox, &[c]).await?;
		t.record_arkoor_receive_outputs(receives[2], mailbox, &[d]).await?;
		t.record_arkoor_receive_outputs(receives[3], mailbox, &[e, f, g]).await?;
		for id in [a, c, d, e, f, g, unrelated] {
			post(t, id, mailbox.as_pubkey(), SIGNED).await?;
		}
		Ok(receives)
	}).await.unwrap();

	let error = certify(&db, &[c]).await.expect_err("certification without a chain tip must fail");
	assert!(format!("{error:#}").contains("no chain tip"), "{error:#}");
	db.write(async |t| {
		t.execute("INSERT INTO captaind_block (height, hash) VALUES (100, $1)", &[&"0".repeat(64)]).await?;
		Ok(())
	}).await.unwrap();

	certify(&db, &[a, b, c, d, e, f, g, unrelated]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, c]));
	assert_eq!(receives_delivered(&db, &receives).await, [Some(false), Some(true), Some(false), Some(false)]);
	let unknown = receives.iter().max().unwrap() + 1;
	assert_eq!(receives_delivered(&db, &[unknown]).await, [None], "no output was recorded for it");

	// The owner can spend a while b is in flight. An unregistered VTXO, or
	// one posted to another mailbox, is not delivered.
	let update = async |sql: &str, id: VtxoId| db.write(async |t| {
		t.execute(sql, &[&id.to_string()]).await?;
		Ok(())
	}).await.unwrap();
	update("UPDATE vtxo SET spend_state = 'spent', updated_at = NOW() WHERE vtxo_id = $1", a).await;
	db.write(async |t| post(t, b, other_mailbox, SIGNED).await).await.unwrap();
	certify(&db, &[b]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, c]), "an unregistered output is not delivered");
	update("UPDATE vtxo SET spend_state = 'spendable', updated_at = NOW() WHERE vtxo_id = $1", b).await;
	certify(&db, &[b]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, c]), "a post to another mailbox delivers nothing");

	// The posted VTXO must be the registered one.
	db.write(async |t| {
		t.execute("UPDATE mailbox SET unblinded_mailbox_id = $2, vtxo = '\\x02'::BYTEA WHERE vtxo_id = $1",
			&[&b.to_string(), &mailbox.to_string()]).await?;
		Ok(())
	}).await.unwrap();
	certify(&db, &[b]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, c]), "the posted bytes must match the registered ones");

	// A round ban does not delay delivery, and the spent a still counts.
	update("UPDATE mailbox SET vtxo = '\\x01'::BYTEA WHERE vtxo_id = $1", b).await;
	update("UPDATE vtxo SET banned_until_height = 101, updated_at = NOW() WHERE vtxo_id = $1", b).await;
	certify(&db, &[b]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, b, c]));
	assert_eq!(receives_delivered(&db, &receives).await, [Some(true), Some(true), Some(false), Some(false)]);

	// Later spends and payout decisions keep the deliveries.
	update("UPDATE vtxo SET spend_state = 'spent', updated_at = NOW() WHERE vtxo_id = $1", c).await;
	db.write(async |t| {
		let payout = t.query_one("INSERT INTO expiry_payout (expiry_height) VALUES (200) RETURNING id", &[])
			.await?.get::<_, i64>(0);
		t.execute("INSERT INTO expiry_payout_vtxo (expiry_payout_id, vtxo_id, disposition,
			gross_sat, fee_sat, payout_sat, replannable) VALUES ($1, $2, 'operator', 10000, 0, 0, FALSE)",
			&[&payout, &a.to_string()]).await?;
		Ok(())
	}).await.unwrap();
	certify(&db, &[a, b, c]).await.unwrap();
	assert_eq!(delivered(&db).await, ids(&[a, b, c]));
	assert_eq!(receives_delivered(&db, &receives).await, [Some(true), Some(true), Some(false), Some(false)]);
}
