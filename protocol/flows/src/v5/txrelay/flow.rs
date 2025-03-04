use crate::{
    flow_context::{FlowContext, RequestScope},
    flow_trait::Flow,
    flowcontext::transactions::MAX_INV_PER_TX_INV_MSG,
};
use kaspa_consensus_core::tx::{Transaction, TransactionId};
use kaspa_consensusmanager::ConsensusProxy;
use kaspa_mining::{
    errors::MiningManagerError,
    mempool::{
        errors::RuleError,
        tx::{Orphan, Priority},
    },
};
use kaspa_p2p_lib::{
    common::{ProtocolError, DEFAULT_TIMEOUT},
    dequeue, make_message,
    pb::{kaspad_message::Payload, RequestTransactionsMessage, TransactionNotFoundMessage},
    IncomingRoute, Router,
};
use std::sync::Arc;
use tokio::time::timeout;

enum Response {
    Transaction(Transaction),
    NotFound(TransactionId),
}

impl Response {
    fn transaction_id(&self) -> TransactionId {
        match self {
            Response::Transaction(tx) => tx.id(),
            Response::NotFound(id) => id.to_owned(),
        }
    }
}

/// Flow listening to InvTransactions messages, requests their corresponding transactions if they
/// are missing, adds them to the mempool and propagates them to the rest of the network.
pub struct RelayTransactionsFlow {
    ctx: FlowContext,
    router: Arc<Router>,
    /// A route specific for invs messages
    invs_route: IncomingRoute,
    /// A route for other messages such as Transaction and TransactionNotFound
    msg_route: IncomingRoute,
}

#[async_trait::async_trait]
impl Flow for RelayTransactionsFlow {
    fn router(&self) -> Option<Arc<Router>> {
        Some(self.router.clone())
    }

    async fn start(&mut self) -> Result<(), ProtocolError> {
        self.start_impl().await
    }
}

impl RelayTransactionsFlow {
    pub fn new(ctx: FlowContext, router: Arc<Router>, invs_route: IncomingRoute, msg_route: IncomingRoute) -> Self {
        Self { ctx, router, invs_route, msg_route }
    }

    pub fn invs_channel_size() -> usize {
        // TODO: reevaluate when the node is fully functional and later when the network tx rate increases
        // Note: in go-kaspad we have 10,000 for this channel combined with tx channel.
        8192
    }

    pub fn txs_channel_size() -> usize {
        // Incoming tx flow capacity must correlate with the max number of invs per tx inv
        // message, since this effectively becomes the upper-bound on number of tx requests
        MAX_INV_PER_TX_INV_MSG
    }

    async fn start_impl(&mut self) -> Result<(), ProtocolError> {
        // trace!("Starting relay transactions flow with {}", self.router.identity());
        loop {
            // Loop over incoming block inv messages
            let inv: Vec<TransactionId> = dequeue!(self.invs_route, Payload::InvTransactions)?.try_into()?;
            // trace!("Receive an inv message from {} with {} transaction ids", self.router.identity(), inv.len());

            if inv.len() > MAX_INV_PER_TX_INV_MSG {
                return Err(ProtocolError::Other("Number of invs in tx inv message is over the limit"));
            }

            let consensus = self.ctx.consensus();
            let session = consensus.session().await;

            // Transaction relay is disabled if the node is out of sync and thus not mining
            if !session.async_is_nearly_synced().await {
                continue;
            }

            let requests = self.request_transactions(inv).await?;
            self.receive_transactions(session, requests).await?;
        }
    }

    async fn request_transactions(
        &self,
        transaction_ids: Vec<TransactionId>,
    ) -> Result<Vec<RequestScope<TransactionId>>, ProtocolError> {
        // Build a vector with the transaction ids unknown in the mempool and not already requested
        // by another peer
        let mut requests = Vec::new();
        for transaction_id in transaction_ids {
            if !self.is_known_transaction(transaction_id).await {
                if let Some(req) = self.ctx.try_adding_transaction_request(transaction_id) {
                    requests.push(req);
                }
            }
        }

        // Request the transactions
        if !requests.is_empty() {
            // TODO: determine if there should be a limit to the number of ids per message
            // trace!("Send a request to {} with {} transaction ids", self.router.identity(), requests.len());
            self.router
                .enqueue(make_message!(
                    Payload::RequestTransactions,
                    RequestTransactionsMessage { ids: requests.iter().map(|x| x.req.into()).collect() }
                ))
                .await?;
        }

        Ok(requests)
    }

    async fn is_known_transaction(&self, transaction_id: TransactionId) -> bool {
        // Ask the transaction memory pool if the transaction is known
        // to it in any form (main pool or orphan).
        self.ctx.mining_manager().clone().has_transaction(transaction_id, true, true).await
    }

    /// Returns the next Transaction or TransactionNotFound message in msg_route,
    /// returning only one of the message types at a time.
    async fn read_response(&mut self) -> Result<Response, ProtocolError> {
        match timeout(DEFAULT_TIMEOUT, self.msg_route.recv()).await {
            Ok(op) => {
                if let Some(msg) = op {
                    match msg.payload {
                        Some(Payload::Transaction(payload)) => Ok(Response::Transaction(payload.try_into()?)),
                        Some(Payload::TransactionNotFound(payload)) => Ok(Response::NotFound(payload.try_into()?)),
                        _ => Err(ProtocolError::UnexpectedMessage(
                            stringify!(Payload::Transaction | Payload::TransactionNotFound),
                            msg.payload.as_ref().map(|v| v.into()),
                        )),
                    }
                } else {
                    Err(ProtocolError::ConnectionClosed)
                }
            }
            Err(_) => {
                // One reason this may happen is the invs_route being full and preventing
                // the router from routing other incoming messages
                Err(ProtocolError::Timeout(DEFAULT_TIMEOUT))
            }
        }
    }

    async fn receive_transactions(
        &mut self,
        consensus: ConsensusProxy,
        requests: Vec<RequestScope<TransactionId>>,
    ) -> Result<(), ProtocolError> {
        // trace!("Receive {} transaction ids from {}", requests.len(), self.router.identity());
        for request in requests {
            let response = self.read_response().await?;
            let transaction_id = response.transaction_id();
            if transaction_id != request.req {
                return Err(ProtocolError::OtherOwned(format!(
                    "requested transaction id {} but got transaction {}",
                    request.req, transaction_id
                )));
            }
            let Response::Transaction(transaction) = response else { continue; };
            match self
                .ctx
                .mining_manager()
                .clone()
                .validate_and_insert_transaction(&consensus, transaction, Priority::Low, Orphan::Allowed)
                .await
            {
                Ok(accepted_transactions) => {
                    // trace!("Broadcast {} accepted transaction ids", accepted_transactions.len());
                    self.ctx.broadcast_transactions(accepted_transactions.iter().map(|x| x.id())).await?;
                }
                Err(MiningManagerError::MempoolError(err)) => {
                    if let RuleError::RejectInvalid(_) = err {
                        // TODO: discuss a banning process
                        return Err(ProtocolError::MisbehavingPeer(format!("rejected invalid transaction {}", transaction_id)));
                    }
                    continue;
                }
                Err(_) => {}
            }
        }
        // trace!("Processed {} transactions from {}", requests.len(), self.router.identity());
        Ok(())
    }
}

// Flow listening to RequestTransactions messages, responding with the requested
// transactions if those are in the mempool.
// Missing transactions would be ignored
pub struct RequestTransactionsFlow {
    ctx: FlowContext,
    router: Arc<Router>,
    incoming_route: IncomingRoute,
}

#[async_trait::async_trait]
impl Flow for RequestTransactionsFlow {
    fn router(&self) -> Option<Arc<Router>> {
        Some(self.router.clone())
    }

    async fn start(&mut self) -> Result<(), ProtocolError> {
        self.start_impl().await
    }
}

impl RequestTransactionsFlow {
    pub fn new(ctx: FlowContext, router: Arc<Router>, incoming_route: IncomingRoute) -> Self {
        Self { ctx, router, incoming_route }
    }

    async fn start_impl(&mut self) -> Result<(), ProtocolError> {
        loop {
            let msg = dequeue!(self.incoming_route, Payload::RequestTransactions)?;
            let tx_ids: Vec<_> = msg.try_into()?;
            for transaction_id in tx_ids {
                if let Some(mutable_tx) = self.ctx.mining_manager().clone().get_transaction(transaction_id, true, false).await {
                    // trace!("Send transaction {} to {}", mutable_tx.id(), self.router.identity());
                    self.router.enqueue(make_message!(Payload::Transaction, (&*mutable_tx.tx).into())).await?;
                } else {
                    // trace!("Send transaction id {} not found to {}", transaction_id, self.router.identity());
                    self.router
                        .enqueue(make_message!(
                            Payload::TransactionNotFound,
                            TransactionNotFoundMessage { id: Some(transaction_id.into()) }
                        ))
                        .await?;
                }
            }
        }
    }
}
