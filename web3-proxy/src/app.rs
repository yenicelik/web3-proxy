use crate::config::Web3ConnectionConfig;
use crate::connections::Web3Connections;
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use dashmap::DashMap;
use ethers::prelude::H256;
use futures::future::join_all;
use linkedhashmap::LinkedHashMap;
use parking_lot::RwLock;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, instrument, trace, warn};

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: put this in config? what size should we do?
const RESPONSE_CACHE_CAP: usize = 1024;

/// TODO: these types are probably very bad keys and values. i couldn't get caching of warp::reply::Json to work
type CacheKey = (Option<H256>, String, Option<String>);

type ResponseLrcCache = RwLock<LinkedHashMap<CacheKey, JsonRpcForwardedResponse>>;

type ActiveRequestsMap = DashMap<CacheKey, watch::Receiver<bool>>;

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Arc<Web3Connections>,
    incoming_requests: ActiveRequestsMap,
    response_cache: ResponseLrcCache,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    pub async fn try_new(
        chain_id: usize,
        redis_address: Option<String>,
        balanced_rpcs: Vec<Web3ConnectionConfig>,
        private_rpcs: Vec<Web3ConnectionConfig>,
    ) -> anyhow::Result<Web3ProxyApp> {
        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

        let rate_limiter = match redis_address {
            Some(redis_address) => {
                info!("Connecting to redis on {}", redis_address);
                let redis_client = redis_cell_client::Client::open(redis_address)?;

                let redis_conn = redis_client.get_multiplexed_tokio_connection().await?;

                Some(redis_conn)
            }
            None => {
                info!("No redis address");
                None
            }
        };

        // TODO: attach context to this error
        let balanced_rpcs = Web3Connections::try_new(
            chain_id,
            balanced_rpcs,
            http_client.as_ref(),
            rate_limiter.as_ref(),
        )
        .await?;

        // TODO: do this separately instead of during try_new
        {
            let balanced_rpcs = balanced_rpcs.clone();
            task::spawn(async move {
                balanced_rpcs.subscribe_heads().await;
            });
        }

        // TODO: attach context to this error
        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            Web3Connections::try_new(
                chain_id,
                private_rpcs,
                http_client.as_ref(),
                rate_limiter.as_ref(),
            )
            .await?
        };

        Ok(Web3ProxyApp {
            balanced_rpcs,
            private_rpcs,
            incoming_requests: Default::default(),
            response_cache: Default::default(),
        })
    }

    pub fn get_balanced_rpcs(&self) -> &Web3Connections {
        &self.balanced_rpcs
    }

    pub fn get_private_rpcs(&self) -> &Web3Connections {
        &self.private_rpcs
    }

    pub fn get_active_requests(&self) -> &ActiveRequestsMap {
        &self.incoming_requests
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    #[instrument(skip_all)]
    pub async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: i don't always see this in the logs. why?
        debug!("Received request: {:?}", request);

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that delays from
        let max_time = Duration::from_secs(60);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(max_time, self.proxy_web3_rpc_request(request)).await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(max_time, self.proxy_web3_rpc_requests(requests)).await??,
            ),
        };

        // TODO: i don't always see this in the logs. why?
        debug!("Forwarding response: {:?}", response);

        Ok(response)
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_requests(
        self: Arc<Web3ProxyApp>,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        // we cut up the request and send to potentually different servers. this could be a problem.
        // if the client needs consistent blocks, they should specify instead of assume batches work on the same
        // TODO: is spawning here actually slower?
        let num_requests = requests.len();
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.clone().proxy_web3_rpc_request(request))
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response?);
        }

        Ok(collected)
    }

    fn get_cached_response(
        &self,
        request: &JsonRpcRequest,
    ) -> (
        CacheKey,
        Result<JsonRpcForwardedResponse, &ResponseLrcCache>,
    ) {
        // TODO: inspect the request to pick the right cache
        // TODO: https://github.com/ethereum/web3.py/blob/master/web3/middleware/cache.py

        // TODO: Some requests should skip caching on the head_block_hash
        let head_block_hash = Some(self.balanced_rpcs.get_head_block_hash());

        // TODO: better key? benchmark this
        let key = (
            head_block_hash,
            request.method.clone(),
            request.params.clone().map(|x| x.to_string()),
        );

        if let Some(response) = self.response_cache.read().get(&key) {
            // TODO: emit a stat
            trace!("{:?} cache hit!", request);

            // TODO: can we make references work? maybe put them in an Arc?
            return (key, Ok(response.to_owned()));
        } else {
            // TODO: emit a stat
            trace!("{:?} cache miss!", request);
        }

        // TODO: multiple caches. if head_block_hash is None, have a persistent cache (disk backed?)
        let cache = &self.response_cache;

        (key, Err(cache))
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_request(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        // TODO: how much should we retry? probably with a timeout and not with a count like this
        // TODO: think more about this loop.
        for _i in 0..10usize {
            // // TODO: add more to this span
            // let span = info_span!("i", ?i);
            // let _enter = span.enter(); // DO NOT ENTER! we can't use enter across awaits! (clippy lint soon)
            if request.method == "eth_sendRawTransaction" {
                // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
                // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
                return self
                    .private_rpcs
                    .try_send_all_upstream_servers(request)
                    .await;
            } else {
                // this is not a private transaction (or no private relays are configured)

                let (cache_key, response_cache) = match self.get_cached_response(&request) {
                    (cache_key, Ok(response)) => {
                        let _ = self.incoming_requests.remove(&cache_key);

                        return Ok(response);
                    }
                    (cache_key, Err(response_cache)) => (cache_key, response_cache),
                };

                // check if this request is already in flight
                // TODO: move this logic into an IncomingRequestHandler (ActiveRequestHandler has an rpc, but this won't)
                let (incoming_tx, incoming_rx) = watch::channel(true);
                let mut other_incoming_rx = None;
                match self.incoming_requests.entry(cache_key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        other_incoming_rx = Some(entry.get().clone());
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(incoming_rx);
                    }
                }

                if let Some(mut other_incoming_rx) = other_incoming_rx {
                    // wait for the other request to finish. it might have finished successfully or with an error
                    trace!("{:?} waiting on in-flight request", request);

                    let _ = other_incoming_rx.changed().await;

                    // now that we've waited, lets check the cache again
                    if let Some(cached) = response_cache.read().get(&cache_key) {
                        let _ = self.incoming_requests.remove(&cache_key);
                        let _ = incoming_tx.send(false);

                        // TODO: emit a stat
                        trace!(
                            "{:?} cache hit after waiting for in-flight request!",
                            request
                        );

                        return Ok(cached.to_owned());
                    } else {
                        // TODO: emit a stat
                        trace!(
                            "{:?} cache miss after waiting for in-flight request!",
                            request
                        );
                    }
                }

                // TODO: move this whole match to a function on self.balanced_rpcs. incoming requests checks makes it awkward
                match self.balanced_rpcs.next_upstream_server().await {
                    Ok(active_request_handle) => {
                        let response = active_request_handle
                            .request(&request.method, &request.params)
                            .await;

                        let response = match response {
                            Ok(partial_response) => {
                                // TODO: trace here was really slow with millions of requests.
                                // trace!("forwarding request from {}", upstream_server);

                                let response = JsonRpcForwardedResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id: request.id,
                                    // TODO: since we only use the result here, should that be all we return from try_send_request?
                                    result: Some(partial_response),
                                    error: None,
                                };

                                // TODO: small race condidition here. parallel requests with the same query will both be saved to the cache
                                let mut response_cache = response_cache.write();

                                // TODO: cache the warp::reply to save us serializing every time
                                response_cache.insert(cache_key.clone(), response.clone());
                                if response_cache.len() >= RESPONSE_CACHE_CAP {
                                    // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
                                    response_cache.pop_front();
                                }

                                drop(response_cache);

                                // TODO: needing to remove manually here makes me think we should do this differently
                                let _ = self.incoming_requests.remove(&cache_key);
                                let _ = incoming_tx.send(false);

                                response
                            }
                            Err(e) => {
                                // send now since we aren't going to cache an error response
                                let _ = incoming_tx.send(false);

                                JsonRpcForwardedResponse::from_ethers_error(e, request.id)
                            }
                        };

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.incoming_requests.remove(&cache_key);

                        if response.error.is_some() {
                            trace!("Sending error reply: {:?}", response);

                            // errors already sent false to the in_flight_tx
                        } else {
                            trace!("Sending reply: {:?}", response);

                            let _ = incoming_tx.send(false);
                        }

                        return Ok(response);
                    }
                    Err(None) => {
                        // TODO: this is too verbose. if there are other servers in other tiers, we use those!
                        warn!("No servers in sync!");

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.incoming_requests.remove(&cache_key);
                        let _ = incoming_tx.send(false);

                        return Err(anyhow::anyhow!("no servers in sync"));
                    }
                    Err(Some(retry_after)) => {
                        // TODO: move this to a helper function
                        // sleep (TODO: with a lock?) until our rate limits should be available
                        // TODO: if a server catches up sync while we are waiting, we could stop waiting
                        warn!("All rate limits exceeded. Sleeping");

                        sleep(retry_after).await;

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.incoming_requests.remove(&cache_key);
                        let _ = incoming_tx.send(false);

                        continue;
                    }
                }
            }
        }

        Err(anyhow::anyhow!("internal error"))
    }
}
