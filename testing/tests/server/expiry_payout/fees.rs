use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bdk_wallet::ChangeSet;
use bitcoin::{FeeRate, Transaction};
use bitcoin::bip32::Xpub;
use bitcoin_ext::{BlockDelta, BlockHeight, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};

use ark::fees::ExpiryPayoutFees;
use server::database::Db;
use server::wallet::WalletKind;

use ark_testing::{sat, TestContext};
use ark_testing::constants::WATCHMAND_REGULAR_FEE_RATE_SAT_VB;
use ark_testing::daemon::captaind::WalletAdminClient;
use ark_testing::util::{poll_interval, FutureExt};
use server_rpc::protos;

use super::{
	idle_board, assert_paid, current_share, fund_watchman, miner_fee, mine, mine_past,
	owner_fee, payout_attempts, payout_fee, payout_output, sync, wait_decided, wait_for_confirmation,
	wait_payout, SPONSORED_WEIGHT_VB,
};

/// A server in expiry payout mode publishes the weight of the payout fee
/// with its other fees, and charges each owner that weight at the regular
/// feerate of the watchman. A server without expiry payouts publishes none.
#[tokio::test]
async fn expiry_payout_fee_is_the_published_weight() {
	let ctx = TestContext::new("server/expiry_payout_fee_is_the_published_weight").await;
	let weight = ExpiryPayoutFees { weight_vb: 150 };
	let published = weight.clone();
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(move |cfg| {
			cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
			cfg.fees.expiry_payout = Some(published);
		})
		.watchmand_cfg(|cfg| cfg.watchman.expiry_payout_weight_vb = 150)
		.create().await;
	assert_eq!(srv.ark_info().await.fees.expiry_payout.as_ref(), Some(&weight));
	// A wallet computes the fee from the weight and a feerate.
	let fee = weight.calculate(FeeRate::from_sat_per_vb_u32(WATCHMAND_REGULAR_FEE_RATE_SAT_VB)).unwrap();
	assert_eq!(fee, sat(1_050));

	let (vtxo, script, _) = idle_board(&ctx, &srv, "owner", sat(100_000)).await;
	fund_watchman(&ctx, &srv, sat(100_000)).await;
	mine_past(&ctx, &srv, vtxo.expiry_height()).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (_, payout) = wait_payout(&db, vtxo.id()).await;
	payout_output(&payout, &script, vtxo.amount() - fee);
	assert_eq!(current_share(&db, vtxo.id()).await.unwrap().fee, fee);

	let upstream = ctx.captaind("upstream").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().create().await;
	assert_eq!(upstream.ark_info().await.fees.expiry_payout, None);
}

/// A pending payout's fee input remains reserved even when the watchman
/// wallet has forgotten that payout while Core still holds it in the mempool.
#[tokio::test]
async fn pending_payout_reserves_its_sponsor_after_wallet_restore() {
	let ctx = TestContext::new("server/pending_payout_reserves_its_sponsor_after_wallet_restore").await;
	ctx.bitcoind().prepare_funds().await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().watchmand()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		// The payout fee does not cover the payout, so it needs a watchman input.
		.watchmand_cfg(|cfg| {
			cfg.watchman.expiry_payout_weight_vb = SPONSORED_WEIGHT_VB;
			cfg.fee_estimator.fallback_fee_rate_regular = FeeRate::from_sat_per_vb(30).unwrap();
		}).create().await;
	let (first, _, _) = idle_board(&ctx, &srv, "first", sat(100_000)).await;
	mine(&ctx, &srv, 3).await;
	let (second, _, _) = idle_board(&ctx, &srv, "second", sat(100_000)).await;
	assert!(first.expiry_height() < second.expiry_height());
	let sponsors = [fund_watchman(&ctx, &srv, sat(100_000)).await,
		fund_watchman(&ctx, &srv, sat(100_000)).await];
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	srv.watchmand().stop().await.unwrap();
	while ctx.bitcoind().get_block_count().await as u32 <= first.expiry_height().to_u32() {
		ctx.mine(std::iter::empty::<&Transaction>()).await;
	}
	srv.watchmand().start().await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	sync(&srv, tip).await;
	let (_, first_tx) = wait_payout(&db, first.id()).await;
	let first_sponsors = first_tx.input.iter().map(|input| input.previous_output)
		.filter(|point| *point != first.chain_anchor()).collect::<BTreeSet<_>>();
	assert_eq!(first_sponsors.len(), 1);
	assert!(sponsors.iter().any(|point| first_sponsors.contains(point)));
	let protected = db.read(async |t| t.stored_expiry_payout_txids(
		&[first_tx.compute_txid(), sponsors[0].txid]).await).await.unwrap();
	assert_eq!(protected.len(), 1);
	assert!(protected.contains(&first_tx.compute_txid()));
	let core = ctx.bitcoind().sync_client();
	async {
		while !core.get_raw_mempool().unwrap().contains(&first_tx.compute_txid()) {
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(30_000).await;

	// Simulate the upstream wallet's mempool eviction by persisting only
	// its eviction marker. Keep every block checkpoint and Core's mempool.
	srv.watchmand().stop().await.unwrap();
	let mut evicted = ChangeSet::default();
	let evicted_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
	evicted.tx_graph.last_evicted.insert(first_tx.compute_txid(), evicted_at);
	db.write(async |t| t.store_changeset(WalletKind::Watchman, &evicted).await).await.unwrap();
	srv.watchmand().start().await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	sync(&srv, tip).await;
	let mut wallet_rpc = WalletAdminClient::connect(format!("http://{}",
		srv.watchmand().config().admin_address.unwrap())).await.unwrap();
	srv.watchmand().trigger_sweep().await;
	async {
		loop {
			let status = wallet_rpc.wallet_status(protos::Empty {}).await.unwrap().into_inner().watchman.unwrap();
			if !status.confirmed_utxos.contains(&first_sponsors.iter().next().unwrap().to_string()) {
				break;
			}
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(5_000).await;
	while ctx.bitcoind().get_block_count().await as u32 <= second.expiry_height().to_u32() {
		let tip = ctx.mine(std::iter::empty::<&Transaction>()).await;
		sync(&srv, tip).await;
	}
	let (_, second_tx) = wait_payout(&db, second.id()).await;
	let second_sponsors = second_tx.input.iter().map(|input| input.previous_output)
		.filter(|point| *point != second.chain_anchor()).collect::<BTreeSet<_>>();
	assert_eq!(second_sponsors.len(), 1);
	assert!(first_sponsors.is_disjoint(&second_sponsors));
	assert!(core.get_raw_mempool().unwrap().contains(&first_tx.compute_txid()));
	let tip = ctx.mine([&first_tx]).await;
	sync(&srv, tip).await;
	let protected = db.read(async |t| t.stored_expiry_payout_txids(
		&[first_tx.compute_txid()]).await).await.unwrap();
	assert!(protected.is_empty(), "confirmed payouts no longer reserve wallet inputs");
}

/// When the feerate rises, the watchman replaces the payout with one that
/// pays the same owner output and records both signed transactions.
#[tokio::test]
async fn expiry_payout_fee_bump_keeps_the_owner_output() {
	let ctx = TestContext::new("server/expiry_payout_fee_bump_keeps_the_owner_output").await;
	let core = ctx.bitcoind().sync_client();
	let descriptor = ctx.import_sweep_descriptor();
	let operator_scripts = core.derive_addresses(&descriptor, Some([0, 1])).unwrap()
		.into_iter().map(|address| address.assume_checked().script_pubkey()).collect::<Vec<_>>();
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().watchmand()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		// A payout weight of 300 vB leaves change for the operator in the first
		// payout.
		.watchmand_cfg(move |cfg| {
			cfg.watchman.expiry_payout_weight_vb = 300;
			cfg.sweep_address = None;
			cfg.sweep_descriptor = Some(descriptor.into());
		}).create().await;
	let paid = sat(100_000) - payout_fee(300, WATCHMAND_REGULAR_FEE_RATE_SAT_VB);
	let (vtxo, script, _) = idle_board(&ctx, &srv, "owner", sat(100_000)).await;
	// Confirmed watchman inputs can pay the higher fee of the replacement.
	fund_watchman(&ctx, &srv, sat(100_000)).await;
	fund_watchman(&ctx, &srv, sat(100_000)).await;
	mine_past(&ctx, &srv, vtxo.expiry_height()).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, first) = wait_payout(&db, vtxo.id()).await;
	let owner_output = payout_output(&first, &script, paid);
	assert!(first.output.iter().any(|out| out.script_pubkey == operator_scripts[0]));
	let target = FeeRate::from_sat_per_vb(100).unwrap();
	assert!(miner_fee(&ctx, &first).await < target * first.weight());

	srv.watchmand().stop().await.unwrap();
	{
		let mut config = srv.watchmand().config_mut();
		config.fee_estimator.fallback_fee_rate_fast = target;
		config.fee_estimator.fallback_fee_rate_regular = target;
		config.fee_estimator.fallback_fee_rate_slow = target;
	}
	srv.watchmand().start().await.unwrap();
	let replacement = async {
		loop {
			let share = current_share(&db, vtxo.id()).await.unwrap();
			let tx = share.tx.unwrap();
			if tx != first && core.get_raw_mempool().unwrap().contains(&tx.compute_txid()) {
				break tx;
			}
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(30_000).await;
	assert!(!core.get_raw_mempool().unwrap().contains(&first.compute_txid()));
	assert_eq!(replacement.output[payout_output(&replacement, &script, paid).vout as usize],
		first.output[owner_output.vout as usize]);
	assert!(replacement.output.iter().any(|out| out.script_pubkey == operator_scripts[1]));
	let fee = miner_fee(&ctx, &replacement).await;
	assert!(fee >= target * replacement.weight());
	let attempts = payout_attempts(&db, allocation).await;
	assert_eq!(attempts.len(), 2, "both signed transactions are kept");
	assert_eq!(attempts[&replacement.compute_txid()], fee);

	assert_paid(&ctx, &srv, &replacement, &script, paid).await;
	let tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let block = core.custom_get_raw_transaction_info(replacement.compute_txid(), None).unwrap().unwrap()
		.blockhash.unwrap();
	wait_for_confirmation(&db, allocation, tip, block).await;
}

/// A feerate estimate above the configured ceiling stops the payout.
/// Raising the ceiling lets the same decision pay.
#[tokio::test]
async fn expiry_payout_above_the_feerate_ceiling_is_held() {
	let ctx = TestContext::new("server/expiry_payout_above_the_feerate_ceiling_is_held").await;
	let ceiling = FeeRate::from_sat_per_vb(5).unwrap();
	let operator = super::Owner::new(&[36; 32]);
	let xpub = Xpub::from_priv(&ark::SECP, &operator.account);
	let descriptor = format!("tr({xpub}/*)");
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		// The watchman decides at 10 sat/vB, so the fee is 2,000 sats.
		.watchmand_cfg(move |cfg| {
			cfg.fee_estimator.fallback_fee_rate_regular = FeeRate::from_sat_per_vb_u32(10);
			cfg.watchman.expiry_payout_max_fee_rate = ceiling;
			cfg.sweep_address = None;
			cfg.sweep_descriptor = Some(descriptor.into());
		}).create().await;
	// Use the operator's first child as the owner's mailbox to force a collision.
	let owner = ctx.bark_sdk("owner", &srv).external_mailbox_keypair(operator.key(0))
		.boarded(sat(100_000)).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	owner.stop_daemon_wait().await.unwrap();
	let vtxo = owner.spendable_vtxos().await.unwrap().pop().unwrap();
	let script = owner.payout_script();
	mine_past(&ctx, &srv, vtxo.expiry_height()).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let allocation = wait_decided(&db, vtxo.id()).await.allocation;
	// Several sweep passes run in this time, and none may sign a payout.
	tokio::time::sleep(Duration::from_secs(5)).await;
	assert!(payout_attempts(&db, allocation).await.is_empty(), "a payout above the ceiling was signed");
	assert_eq!(db.read(async |t| t.next_sweep_index().await).await.unwrap(), 0);

	srv.watchmand().stop().await.unwrap();
	{
		let mut cfg = srv.watchmand().config_mut();
		cfg.watchman.expiry_payout_max_fee_rate = FeeRate::from_sat_per_vb(1000).unwrap();
		// The board covers the fee at 12 sat/vB, but change would be dust.
		cfg.fee_estimator.fallback_fee_rate_regular = FeeRate::from_sat_per_vb(12).unwrap();
	}
	srv.watchmand().start().await.unwrap();
	let (paid, payout) = wait_payout(&db, vtxo.id()).await;
	assert_eq!(paid, allocation);
	assert!(miner_fee(&ctx, &payout).await > ceiling * payout.weight());
	payout_output(&payout, &script, sat(100_000) - owner_fee(10));
	assert_eq!(payout.output.len(), 1);
	let next_index = db.read(async |t| t.next_sweep_index().await).await.unwrap();
	assert_eq!(next_index, 1, "child 0 was paid to the owner and child 1 was never used");
}

/// A decision that cannot be paid yet survives a watchman crash. A payout
/// whose fee needs a watchman input waits until that input is trusted.
#[tokio::test]
async fn expiry_payout_waits_for_a_trusted_fee_input() {
	let ctx = TestContext::new("server/expiry_payout_waits_for_a_trusted_fee_input").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| {
			cfg.min_trusted_confs = 2;
			cfg.watchman.sweep_interval = Duration::from_secs(1);
			// The payout fee does not cover the payout.
			cfg.watchman.expiry_payout_weight_vb = SPONSORED_WEIGHT_VB;
			cfg.fee_estimator.fallback_fee_rate_regular = FeeRate::from_sat_per_vb(30).unwrap();
		}).create().await;
	let (vtxo, script, _) = idle_board(&ctx, &srv, "owner", sat(100_000)).await;
	mine_past(&ctx, &srv, vtxo.expiry_height()).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let decided = wait_decided(&db, vtxo.id()).await;
	assert_eq!(decided.status, "pending");
	assert!(!decided.published);

	srv.watchmand().crash_stop().await.unwrap();
	srv.watchmand().start().await.unwrap();
	let address = srv.watchmand().wait_wallet_address().await;
	let txid = ctx.bitcoind().fund_addr(&address, sat(100_000)).await;
	let deposit = ctx.bitcoind().await_transaction(txid).await;
	let fee_input = bitcoin::OutPoint::new(txid,
		deposit.output.iter().position(|out| out.script_pubkey == address.script_pubkey()).unwrap() as u32);
	mine(&ctx, &srv, 1).await;
	srv.watchmand().trigger_sweep().await;
	// One confirmation is not trusted, so no sweep pass may pay yet.
	tokio::time::sleep(Duration::from_secs(2)).await;
	assert_eq!(current_share(&db, vtxo.id()).await.unwrap(), decided);

	mine(&ctx, &srv, 1).await;
	let (allocation, payout) = wait_payout(&db, vtxo.id()).await;
	assert_eq!(allocation, decided.allocation);
	assert!(payout.input.iter().any(|input| input.previous_output == vtxo.chain_anchor()));
	assert!(payout.input.iter().any(|input| input.previous_output == fee_input));
	assert!(payout.output.iter().all(|out| !out.is_p2a_fee_anchor()));
	let fee = miner_fee(&ctx, &payout).await;
	let owner_fee = payout_fee(SPONSORED_WEIGHT_VB, 30);
	assert!(fee > owner_fee);
	assert_eq!(payout_attempts(&db, allocation).await.into_values().collect::<Vec<_>>(), vec![fee]);
	assert_paid(&ctx, &srv, &payout, &script, sat(100_000) - owner_fee).await;
	let share = current_share(&db, vtxo.id()).await.unwrap();
	assert_eq!((share.status.as_str(), share.fee, share.payout), ("confirmed", owner_fee, sat(100_000) - owner_fee));
}
