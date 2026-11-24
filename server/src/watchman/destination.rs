use anyhow::Context;
use bdk_wallet::miniscript::descriptor::Wildcard;
use bdk_wallet::miniscript::{Descriptor, DescriptorPublicKey, ForEachKey};
use bitcoin::{Address, NetworkKind, ScriptBuf, TxOut};

use crate::config::watchmand::Config;
use crate::database::Tx;
use crate::SECP;

/// Where the operator receives proceeds when watchmand claims a VTXO.
pub enum SweepDestination {
	Fixed(ScriptBuf),
	Ranged(Descriptor<DescriptorPublicKey>),
}

impl From<Address> for SweepDestination {
	fn from(address: Address) -> Self { Self::Fixed(address.script_pubkey()) }
}

impl SweepDestination {
	pub(super) fn from_config(cfg: &Config) -> anyhow::Result<Self> {
		match (&cfg.sweep_address, &cfg.sweep_descriptor) {
			(Some(addr), None) => Ok(Self::Fixed(addr.clone().require_network(cfg.network)
				.context("sweep_address network mismatch")?.script_pubkey())),
			(None, Some(desc)) => {
				// A parse error may contain part of a key. Keep the raw input out of logs.
				let desc = desc.leak_ref().parse::<Descriptor<DescriptorPublicKey>>()
					.map_err(|_| anyhow!("invalid public sweep_descriptor"))?;
				ensure!(desc.has_wildcard() && !desc.is_multipath(),
					"sweep_descriptor must describe one ranged receive branch");
				ensure!(desc.for_each_key(|key| match key {
					DescriptorPublicKey::XPub(key) => key.xkey.network == NetworkKind::from(cfg.network),
					DescriptorPublicKey::Single(_) => true,
					DescriptorPublicKey::MultiXPub(_) => false,
				}), "sweep_descriptor network mismatch");
				// Miniscript assumes definite keys can be derived. Check hardened
				// suffixes and BIP32's maximum depth before asking it to derive.
				ensure!(desc.for_each_key(|key| match key {
					DescriptorPublicKey::XPub(key) => key.wildcard != Wildcard::Hardened
						&& key.derivation_path.into_iter().all(|step| step.is_normal())
						&& usize::from(key.xkey.depth) + key.derivation_path.len()
							+ usize::from(key.wildcard != Wildcard::None) <= usize::from(u8::MAX),
					_ => true,
				}), "sweep_descriptor path must be publicly derivable within BIP32's depth limit");
				desc.sanity_check().context("invalid sweep_descriptor policy")?;
				desc.derived_descriptor(&SECP, 0).context("sweep_descriptor must be publicly derivable")?
					.address(cfg.network).context("sweep_descriptor must generate addresses")?;
				Ok(Self::Ranged(desc))
			}
			_ => bail!("configure exactly one of sweep_address or sweep_descriptor"),
		}
	}

	/// A fixed destination never touches the cursor. For a descriptor, skip any address
	/// that is already owed an owner in this payout, so the operator output is unambiguous.
	pub(super) async fn script(&self, tx: &Tx<'_>, owners: &[TxOut]) -> anyhow::Result<(Option<u32>, ScriptBuf)> {
		match self {
			Self::Fixed(script) => {
				ensure!(!owners.iter().any(|o| &o.script_pubkey == script), "operator drain aliases an owner script");
				Ok((None, script.clone()))
			}
			Self::Ranged(desc) => {
				let next = tx.next_sweep_index().await?;
				for index in next..(1 << 31) {
					let script = desc.derived_descriptor(&SECP, index)?.script_pubkey();
					if !owners.iter().any(|o| o.script_pubkey == script) {
						// The skipped children are being paid as owner outputs even when
						// the payout has no operator change. Retire them on commit.
						if index > next {
							tx.advance_sweep_index(index - 1).await?;
						}
						return Ok((Some(index), script));
					}
				}
				bail!("sweep_descriptor has exhausted its unhardened derivation range")
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitcoin::bip32::{Xpriv, Xpub};
	use bitcoin::Network;

	#[test]
	fn sweep_destination_config() {
		let mut cfg = Config::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("watchmand.default.toml")).unwrap();
		cfg.network = Network::Bitcoin;
		let private = Xpriv::new_master(cfg.network, &[42; 32]).unwrap();
		let xpub = Xpub::from_priv(&SECP, &private);
		let address = Address::p2tr(&SECP, xpub.public_key.x_only_public_key().0, None, cfg.network);
		cfg.sweep_address = None;
		cfg.sweep_descriptor = None;
		assert!(SweepDestination::from_config(&cfg).is_err());
		cfg.sweep_address = Some(address.to_string().parse().unwrap());
		assert!(matches!(SweepDestination::from_config(&cfg), Ok(SweepDestination::Fixed(_))));
		cfg.sweep_descriptor = Some(format!("wpkh({xpub}/0/*)").into());
		assert!(SweepDestination::from_config(&cfg).is_err());
		cfg.sweep_address = None;
		assert!(matches!(SweepDestination::from_config(&cfg), Ok(SweepDestination::Ranged(_))));
		cfg.network = Network::Signet;
		assert!(SweepDestination::from_config(&cfg).is_err());
		cfg.network = Network::Bitcoin;
		let mut deepest = xpub;
		deepest.depth = u8::MAX;
		for desc in [
			format!("tr({deepest}/*)"),
			format!("wpkh({xpub}/0/0)"), format!("tr({xpub}/<0;1>/*)"),
			format!("tr({xpub}/0/*h)"), format!("tr({xpub}/0h/*)"),
			format!("wpkh({private}/0/*)"), format!("tr({xpub}/0/*)#badcheck"),
			format!("pk({xpub}/0/*)"),
		] {
			cfg.sweep_descriptor = Some(desc.into());
			assert!(SweepDestination::from_config(&cfg).is_err());
		}
	}
}
