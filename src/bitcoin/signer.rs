use crate::app::App;
use crate::error::Result;
use bitcoin::secp256k1::{Message, Secp256k1};
use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey, ExtendedPubKey};
use orga::{abci::TendermintClient, coins::Address};
use rand::Rng;
use std::fs;
use std::path::Path;

pub struct Signer {
    op_addr: Address,
    client: TendermintClient<App>,
    xpriv: ExtendedPrivKey,
}

impl Signer {
    pub fn load_or_generate<P: AsRef<Path>>(
        op_addr: Address,
        client: TendermintClient<App>,
        key_path: P,
    ) -> Result<Self> {
        let path = key_path.as_ref();
        let xpriv = if path.exists() {
            println!("Loading signatory key from {}", path.display());
            let bytes = fs::read(path)?;
            let text = String::from_utf8(bytes).unwrap();
            text.trim().parse()?
        } else {
            println!("Generating signatory key at {}", path.display());
            let seed: [u8; 32] = rand::thread_rng().gen();
            // TODO: get network from somewhere
            let xpriv = ExtendedPrivKey::new_master(bitcoin::Network::Testnet, seed.as_slice())?;

            fs::write(path, xpriv.to_string().as_bytes())?;

            xpriv
        };

        let secp = bitcoin::secp256k1::Secp256k1::signing_only();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);
        println!("Signatory xpub:\n{}", xpub);

        Ok(Self::new(op_addr, client, xpriv))
    }

    pub fn new(op_addr: Address, client: TendermintClient<App>, xpriv: ExtendedPrivKey) -> Self {
        Signer {
            op_addr,
            client,
            xpriv,
        }
    }

    pub async fn start(mut self) -> Result<()> {
        let secp = Secp256k1::signing_only();
        let xpub = ExtendedPubKey::from_priv(&secp, &self.xpriv);

        loop {
            self.maybe_submit_xpub(&xpub).await?;

            if let Err(e) = self.try_sign(&xpub).await {
                eprintln!("Signer error: {}", e);
            }

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn maybe_submit_xpub(&mut self, xpub: &ExtendedPubKey) -> Result<()> {
        let cons_key = self.client.staking.consensus_key(self.op_addr).await??;
        let onchain_xpub = self.client.bitcoin.signatory_keys.get(cons_key).await??;

        match onchain_xpub {
            None => self.submit_xpub(xpub).await,
            Some(onchain_xpub) if onchain_xpub.inner() != xpub => Err(orga::Error::App(
                "Local xpub does not match xpub found on chain".to_string(),
            )
            .into()),
            Some(_) => Ok(()),
        }
    }

    async fn submit_xpub(&mut self, xpub: &ExtendedPubKey) -> Result<()> {
        self.client
            .pay_from(async move |client| client.bitcoin.set_signatory_key(xpub.into()).await)
            .noop()
            .await?;
        println!("Submitted signatory key.");
        Ok(())
    }

    async fn try_sign(&mut self, xpub: &ExtendedPubKey) -> Result<()> {
        let secp = Secp256k1::signing_only();

        if self.client.bitcoin.checkpoints.signing().await??.is_none() {
            return Ok(());
        }

        let to_sign = self
            .client
            .bitcoin
            .checkpoints
            .to_sign(xpub.into())
            .await??;
        if to_sign.is_empty() {
            return Ok(());
        }

        println!("Signing checkpoint... ({} inputs)", to_sign.len());

        let sigs: Vec<_> = to_sign
            .into_iter()
            .map(|(msg, index)| {
                let privkey = self
                    .xpriv
                    .derive_priv(&secp, &[ChildNumber::from_normal_idx(index)?])?
                    .private_key;

                Ok(secp
                    .sign_ecdsa(&Message::from_slice(&msg[..])?, &privkey)
                    .serialize_compact())
            })
            .collect::<Result<_>>()?;

        self.client
            .clone()
            .pay_from(async move |client| {
                client
                    .bitcoin
                    .checkpoints
                    .sign(xpub.into(), sigs.into())
                    .await
            })
            .noop()
            .await?;

        println!("Submitted signatures");

        Ok(())
    }
}
