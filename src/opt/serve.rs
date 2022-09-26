use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use futures::{stream::FuturesUnordered, StreamExt, TryStreamExt};
use penumbra_crypto::{Value, Zero};
use penumbra_custody::SoftHSM;
use penumbra_proto::{
    custody::v1alpha1::{
        custody_protocol_client::CustodyProtocolClient,
        custody_protocol_server::CustodyProtocolServer,
    },
    view::v1alpha1::{
        view_protocol_client::ViewProtocolClient, view_protocol_server::ViewProtocolServer,
    },
};
use penumbra_view::{ViewClient, ViewService};
use std::{env, path::PathBuf, time::Duration};

use crate::{
    opt::ChannelIdAndMessageId, responder::RequestQueue, wallet::WalletWorker, Catchup, Handler,
    Responder, Wallet,
};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
    /// The transaction fee for each response (paid in upenumbra).
    #[structopt(long, default_value = "0")]
    fee: u64,
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1day", parse(try_from_str = humantime::parse_duration))]
    rate_limit: Duration,
    /// Maximum number of times to reply to a user informing them of the rate limit.
    #[clap(long, default_value = "5")]
    reply_limit: usize,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(long, default_value = "1")]
    max_addresses: usize,
    /// Path to the directory to use to store data [default: platform appdata directory].
    #[clap(long, short)]
    data_dir: Option<PathBuf>,
    /// The address of the pd+tendermint node.
    #[clap(short, long, default_value = "testnet.penumbra.zone")]
    node: String,
    /// The port to use to speak to pd's gRPC server.
    #[clap(long, default_value = "8080")]
    pd_port: u16,
    /// The port to use to speak to tendermint.
    #[clap(long, default_value = "26657")]
    rpc_port: u16,
    /// The source address index in the wallet to use when dispensing tokens (if unspecified uses
    /// any funds available).
    #[clap(long = "source")]
    source_address: Option<u64>,
    /// Message/channel ids to catch up on backlog from (can be specified as
    /// `<channel_id>/<message_id>` or a full URL as generated by Discord).
    #[clap(long)]
    catch_up: Vec<ChannelIdAndMessageId>,
    /// Batch size for responding to catch-up backlog.
    #[clap(long, default_value = "25")]
    catch_up_batch_size: usize,
    /// The amounts to send for each response, written as typed values 1.87penumbra, 12cubes, etc.
    values: Vec<Value>,
}

impl Serve {
    pub async fn exec(self) -> anyhow::Result<()> {
        if self.values.is_empty() {
            anyhow::bail!("at least one value must be provided");
        } else if self.values.iter().any(|v| v.amount.inner.is_zero()) {
            anyhow::bail!("all values must be non-zero");
        }

        let discord_token =
            env::var("DISCORD_TOKEN").context("missing environment variable DISCORD_TOKEN")?;

        // Look up the path to the view state file per platform, creating the directory if needed
        let data_dir = self.data_dir.unwrap_or_else(|| {
            ProjectDirs::from("zone", "penumbra", "pcli")
                .expect("can access penumbra project dir")
                .data_dir()
                .to_owned()
        });
        std::fs::create_dir_all(&data_dir).context("can create data dir")?;

        let view_file = data_dir.clone().join("pcli-view.sqlite");
        let custody_file = data_dir.clone().join("custody.json");

        // Build a custody service...
        let wallet = Wallet::load(custody_file)?;
        let soft_hsm = SoftHSM::new(vec![wallet.spend_key.clone()]);
        let custody = CustodyProtocolClient::new(CustodyProtocolServer::new(soft_hsm));

        let fvk = wallet.spend_key.full_viewing_key().clone();

        // Instantiate an in-memory view service.
        let view_storage = penumbra_view::Storage::load_or_initialize(
            view_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Non-UTF8 view path"))?
                .to_string(),
            &fvk,
            self.node.clone(),
            self.pd_port,
        )
        .await?;
        let view_service =
            ViewService::new(view_storage, self.node.clone(), self.pd_port, self.rpc_port).await?;

        // Now build the view and custody clients, doing gRPC with ourselves
        let mut view = ViewProtocolClient::new(ViewProtocolServer::new(view_service));

        // Wait to synchronize the chain before doing anything else.
        tracing::info!(
            "starting initial sync: please wait for sync to complete before requesting tokens"
        );
        ViewClient::status_stream(&mut view, fvk.hash())
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        // From this point on, the view service is synchronized.

        // Make a worker to handle the wallet
        let (wallet_requests, wallet_worker) = WalletWorker::new(
            view,
            custody,
            fvk,
            self.source_address,
            self.node,
            self.rpc_port,
        );

        // Make a worker to handle the address queue
        let (send_requests, responder) =
            Responder::new(wallet_requests, self.max_addresses, self.values, self.fee);

        let handler = Handler::new(self.rate_limit, self.reply_limit);

        // Make a new client using a token set by an environment variable, with our handlers
        let mut client = serenity::Client::builder(&discord_token, Default::default())
            .event_handler(handler)
            .await?;

        // Put the sending end of the address queue into the global TypeMap
        client
            .data
            .write()
            .await
            .insert::<RequestQueue>(send_requests.clone());

        // Make a separate catch-up worker for each catch-up task, and collect their results (first
        // to fail kills the bot)
        let http = client.cache_and_http.http.clone();
        let catch_up = tokio::spawn(async move {
            let mut catch_ups: FuturesUnordered<_> = self
                .catch_up
                .into_iter()
                .map(
                    |ChannelIdAndMessageId {
                         channel_id,
                         message_id,
                     }| {
                        let catch_up = Catchup::new(
                            channel_id,
                            self.catch_up_batch_size,
                            http.clone(),
                            send_requests.clone(),
                        );
                        tokio::spawn(catch_up.run(message_id))
                    },
                )
                .collect();

            while let Some(result) = catch_ups.next().await {
                result??;
            }

            // Wait forever
            std::future::pending().await
        });

        // Start the client and the two workers
        tokio::select! {
            result = tokio::spawn(async move { client.start().await }) =>
                result.unwrap().context("error in discord client service"),
            result = tokio::spawn(async move { responder.run().await }) =>
                result.unwrap().context("error in responder service"),
            result = wallet_worker.run() => result.context("error in wallet service"),
            result = catch_up => result.context("error in catchup service")?,
        }
    }
}
