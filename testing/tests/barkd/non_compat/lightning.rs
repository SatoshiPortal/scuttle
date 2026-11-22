use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::Amount;
use bitcoin::secp256k1::Keypair;

use ark::{ProtocolEncoding, VtxoPolicy};
use ark::address::VtxoDelivery;
use ark::arkoor::ArkoorDestination;
use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::arkoor::state::UserGeneratedNonces;
use ark::lightning::{Bolt11InvoiceExt, PaymentHash};
use bark::lightning_invoice::Bolt11Invoice;
use bark_json::cli::LightningReceiveStateKind;
use bark_rest_client::apis::lightning_api;
use bark_rest_client::models::LightningInvoiceForAddressRequest;
use server::database::Db;
use server_rpc::protos;
use server_rpc::protos::LightningReceiveStatus;

use ark_testing::{btc, sat, Captaind, TestContext};
use ark_testing::context::LightningPaymentSetup;
use ark_testing::daemon::barkd::Barkd;
use ark_testing::util::{poll_interval, FutureExt};

/// A server that puts Ark addresses in invoices, in expiry payout mode if
/// `expiry_payouts`, a barkd that forwards Lightning receives, a payer with
/// a board of 400,000 sats, and an owner that receives the forwarded
/// payments and reads its mailbox only when the test asks.
///
/// barkd syncs every five seconds, as a forwarding service configures it.
async fn forward_setup(
	ctx: &TestContext, expiry_payouts: bool,
) -> (LightningPaymentSetup, Arc<Captaind>, Barkd, bark::Wallet, bark::Wallet) {
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(move |cfg| {
			cfg.expiry_payouts = expiry_payouts;
			cfg.ln_receive_ark_address_hints = true;
		})
		.create().await;
	srv.wait_for_vtxopool(ctx).await;
	let forwarder = ctx.barkd("forwarder", &srv)
		.cfg(|cfg| cfg.daemon_sync_interval_secs = 5)
		.create().await;
	let payer = ctx.bark_sdk("payer", &srv).boarded(sat(400_000)).create().await;
	let owner = ctx.bark_sdk("owner", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	lightning.sync().await;
	(lightning, srv, forwarder, payer, owner)
}

/// An invoice of `amount` from `forwarder` that forwards the payment to
/// `destination`, with `destination` in its route hints if `hint`.
async fn forwarded_invoice(
	forwarder: &Barkd, destination: &ark::Address, amount: Amount, hint: bool,
) -> Bolt11Invoice {
	let info = lightning_api::generate_invoice_for_address(
		&forwarder.client_config(),
		LightningInvoiceForAddressRequest {
			amount_sat: amount.to_sat(),
			address: destination.to_string(),
			description: None,
			expiry_secs: None,
			ark_address_hint: hint,
		},
	).await.expect("barkd creates the forwarded invoice");
	let invoice = Bolt11Invoice::from_str(&info.invoice).unwrap();
	let expected = if hint { Some(destination) } else { None };
	assert_eq!(invoice.ark_address().as_ref(), expected);
	invoice
}

/// The state of the receive of `hash` that barkd reports.
async fn receive_state(forwarder: &Barkd, hash: PaymentHash) -> LightningReceiveStateKind {
	lightning_api::get_receive_status(&forwarder.client_config(), &hash.to_string()).await
		.expect("barkd knows the receive").state
}

/// Wait until barkd reports the receive of `hash` in `state`. The daemon
/// drives the receive.
async fn wait_receive_state(forwarder: &Barkd, hash: PaymentHash, state: LightningReceiveStateKind) {
	async {
		while receive_state(forwarder, hash).await != state {
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(60_000).await;
}

/// The status of the receive of `hash` on the server, and whether the
/// server reports the arkoor that paid it delivered.
async fn server_receive(srv: &Captaind, hash: PaymentHash) -> (LightningReceiveStatus, Option<bool>) {
	let resp = srv.get_public_rpc().await
		.check_lightning_receive(protos::CheckLightningReceiveRequest { hash: hash.to_vec(), wait: false })
		.await.unwrap().into_inner();
	(LightningReceiveStatus::try_from(resp.status).unwrap(), resp.delivered)
}

/// The blinded mailbox of `address`.
fn blinded_id(address: &ark::Address) -> Vec<u8> {
	let [VtxoDelivery::ServerMailbox { blinded_id }] = address.delivery() else {
		panic!("address {address} has no single mailbox delivery");
	};
	blinded_id.as_ref().to_vec()
}

/// An arkoor of `amount` from a VTXO of `payer` to `policy`, with that
/// output bound to the mailbox of `bound_to` and the change to a new
/// address of `payer`. Returns the builder, the key of the input and the
/// co-sign request.
async fn arkoor_to(
	payer: &bark::Wallet, policy: VtxoPolicy, bound_to: &ark::Address, amount: Amount,
) -> (ArkoorPackageBuilder<UserGeneratedNonces>, Keypair, protos::ArkoorPackageCosignRequest) {
	let vtxo = payer.spendable_vtxos().await.unwrap().pop().unwrap();
	let vtxo = payer.get_full_vtxo(vtxo.id()).await.unwrap();
	let key = payer.get_vtxo_key(&vtxo).await.unwrap();
	let change = payer.new_address().await.unwrap();
	let outputs = vec![
		ArkoorDestination { total_amount: amount, policy },
		ArkoorDestination { total_amount: vtxo.amount() - amount, policy: change.policy().clone() },
	];
	let builder = ArkoorPackageBuilder::new([vtxo], outputs).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in request.parts.iter_mut().flat_map(|p| p.outputs.iter_mut().chain(&mut p.isolated_outputs)) {
		let is_change = VtxoPolicy::deserialize(&output.policy).unwrap() == *change.policy();
		output.blinded_mailbox_id = blinded_id(if is_change { &change } else { bound_to });
	}
	(builder, key, request)
}

/// A forwarded invoice that carries its destination is paid once over
/// Lightning and once by arkoor from a wallet on the same server. Both
/// payments reach the mailbox of the destination, both settle the receive
/// of barkd, and barkd never holds the payment.
#[tokio::test]
async fn forwarded_invoice_with_destination_hint() {
	let ctx = TestContext::new("barkd/forwarded_invoice_with_destination_hint").await;
	let (lightning, srv, forwarder, payer, owner) = forward_setup(&ctx, true).await;
	let destination = owner.new_address().await.unwrap();
	let amount = sat(100_000);

	// Over Lightning, barkd claims the payment and posts it to the owner.
	let invoice = forwarded_invoice(&forwarder, &destination, amount, true).await;
	let hash = PaymentHash::from(&invoice);
	lightning.external.pay_bolt11(invoice.to_string()).await;
	wait_receive_state(&forwarder, hash, LightningReceiveStateKind::Settled).await;
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Settled, None),
		"a payment over Lightning has no arkoor delivery to track");
	owner.sync().await;
	assert_eq!(owner.balance().await.unwrap().spendable, amount);

	// By arkoor, the payer pays the owner directly.
	let invoice = forwarded_invoice(&forwarder, &destination, amount, true).await;
	let hash = PaymentHash::from(&invoice);
	payer.pay_lightning_invoice(invoice.clone(), None, true).await.unwrap();
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Settled, Some(true)));
	wait_receive_state(&forwarder, hash, LightningReceiveStateKind::Settled).await;
	owner.sync().await;
	assert_eq!(owner.balance().await.unwrap().spendable, amount * 2);
	assert_eq!(payer.balance().await.unwrap().spendable, sat(400_000) - amount);

	assert_eq!(forwarder.bark_balance().await.spendable, Amount::ZERO, "barkd never holds the payments");
	lightning.external.try_pay_bolt11(invoice.to_string()).await
		.expect_err("paid over Lightning after it was paid by arkoor");
}

/// An arkoor to a forwarded invoice must pay the key of the destination,
/// bound to its mailbox. The receive of barkd waits in AwaitingDelivery
/// while the payer stops after the co-sign, and while the arkoor is posted
/// to another mailbox. It settles once the payer registers the arkoor and
/// posts it to the mailbox of the destination, within about one sync
/// interval of barkd.
#[tokio::test]
async fn forwarded_arkoor_receive_settles_once_delivered() {
	let ctx = TestContext::new("barkd/forwarded_arkoor_receive_settles_once_delivered").await;
	let (_lightning, srv, forwarder, payer, owner) = forward_setup(&ctx, true).await;
	let destination = owner.new_address().await.unwrap();
	let amount = sat(100_000);
	let invoice = forwarded_invoice(&forwarder, &destination, amount, true).await;
	let hash = PaymentHash::from(&invoice);
	let mut rpc = srv.get_public_rpc().await;
	let pay = |request| protos::LightningReceiveArkoorCosignRequest {
		payment_hash: hash.to_vec(), cosign_request: Some(request),
	};

	// The claim key of barkd is not the destination.
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |t| t.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	let claim_key = VtxoPolicy::new_pubkey(sub.claim_pubkey.unwrap());
	let (_, _, request) = arkoor_to(&payer, claim_key, &destination, amount).await;
	let error = rpc.request_lightning_receive_arkoor_cosign(pay(request)).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error}");

	// Neither is another mailbox for the key of the destination.
	let other = payer.new_address().await.unwrap();
	let (_, _, request) = arkoor_to(&payer, destination.policy().clone(), &other, amount).await;
	let error = rpc.request_lightning_receive_arkoor_cosign(pay(request)).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error}");
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Created, None));

	// The payer stops after the co-sign.
	let (arkoor, key, request) = arkoor_to(&payer, destination.policy().clone(), &destination, amount).await;
	let response = rpc.request_lightning_receive_arkoor_cosign(pay(request)).await.unwrap().into_inner();
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Settled, Some(false)));
	wait_receive_state(&forwarder, hash, LightningReceiveStateKind::AwaitingDelivery).await;

	let vtxos = arkoor.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap()).unwrap()
		.build_signed_vtxos();
	let paid = vtxos.iter().filter(|v| v.policy() == destination.policy())
		.map(|v| v.serialize()).collect::<Vec<_>>();
	payer.register_vtxo_transactions_with_server(&vtxos).await.unwrap();
	assert_eq!(server_receive(&srv, hash).await.1, Some(false), "the arkoor is not posted yet");

	// A post to another mailbox is refused and delivers nothing.
	let mut mailbox = srv.get_mailbox_public_rpc().await;
	let error = mailbox.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: blinded_id(&other), vtxos: paid.clone(),
	}).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error}");
	assert_eq!(server_receive(&srv, hash).await.1, Some(false));
	assert_eq!(receive_state(&forwarder, hash).await, LightningReceiveStateKind::AwaitingDelivery);

	// The payer resumes and posts it to the mailbox of the destination.
	mailbox.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: blinded_id(&destination), vtxos: paid,
	}).await.unwrap();
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Settled, Some(true)));
	// The sync of barkd drives the receive, so it settles within about one
	// sync interval.
	let delivered = Instant::now();
	wait_receive_state(&forwarder, hash, LightningReceiveStateKind::Settled).await;
	let latency = delivered.elapsed();
	println!("barkd settled the receive {latency:?} after the server reported it delivered");
	assert!(latency < Duration::from_secs(15), "barkd took {latency:?}");
	owner.sync().await;
	assert_eq!(owner.balance().await.unwrap().spendable, amount);

	// The receive stays delivered after the owner spends the payment.
	owner.send_arkoor_payment(&payer.new_address().await.unwrap(), amount).await.unwrap();
	assert_eq!(server_receive(&srv, hash).await.1, Some(true));
}

/// A server that is not in expiry payout mode does not track delivery.
/// barkd settles a forwarded receive paid by arkoor as soon as the server
/// settles it, and a forwarded invoice without the hint has no Ark address.
#[tokio::test]
async fn forwarded_arkoor_receive_without_delivery_tracking() {
	let ctx = TestContext::new("barkd/forwarded_arkoor_receive_without_delivery_tracking").await;
	let (_lightning, srv, forwarder, payer, owner) = forward_setup(&ctx, false).await;
	let destination = owner.new_address().await.unwrap();
	let amount = sat(100_000);
	forwarded_invoice(&forwarder, &destination, amount, false).await;

	let invoice = forwarded_invoice(&forwarder, &destination, amount, true).await;
	let hash = PaymentHash::from(&invoice);
	payer.pay_lightning_invoice(invoice, None, true).await.unwrap();
	assert_eq!(server_receive(&srv, hash).await, (LightningReceiveStatus::Settled, None));
	wait_receive_state(&forwarder, hash, LightningReceiveStateKind::Settled).await;
	owner.sync().await;
	assert_eq!(owner.balance().await.unwrap().spendable, amount);
	assert_eq!(forwarder.bark_balance().await.spendable, Amount::ZERO);
}
