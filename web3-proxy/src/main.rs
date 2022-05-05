mod config;
mod connection;
mod connections;

use config::Web3ConnectionConfig;
use futures::future;
use governor::clock::{Clock, QuantaClock};
use linkedhashmap::LinkedHashMap;
use serde_json::json;
use std::fmt;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{info, warn};
use warp::Filter;
use warp::Reply;

use crate::config::{CliConfig, RpcConfig};
use crate::connection::JsonRpcRequest;
use crate::connections::Web3Connections;

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

const RESPONSE_CACHE_CAP: usize = 128;

/// The application
// TODO: this debug impl is way too verbose. make something smaller
pub struct Web3ProxyApp {
    /// clock used for rate limiting
    /// TODO: use tokio's clock (will require a different ratelimiting crate)
    clock: QuantaClock,
    /// Send requests to the best server available
    balanced_rpc_tiers: Vec<Arc<Web3Connections>>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Option<Arc<Web3Connections>>,
    /// TODO: these types are probably very bad keys and values. i couldn't get caching of warp::reply::Json to work
    response_cache: RwLock<LinkedHashMap<(u64, String, String), serde_json::Value>>,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    async fn try_new(
        balanced_rpc_tiers: Vec<Vec<Web3ConnectionConfig>>,
        private_rpcs: Vec<Web3ConnectionConfig>,
    ) -> anyhow::Result<Web3ProxyApp> {
        let clock = QuantaClock::default();

        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(300))
            .user_agent(APP_USER_AGENT)
            .build()?;

        // TODO: attach context to this error?
        let balanced_rpc_tiers =
            future::join_all(balanced_rpc_tiers.into_iter().map(|balanced_rpc_tier| {
                Web3Connections::try_new(balanced_rpc_tier, Some(http_client.clone()), &clock)
            }))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<Arc<Web3Connections>>>>()?;

        // TODO: attach context to this error?
        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            // TODO: instead of None, set it to a list of all the rpcs from balanced_rpc_tiers. that way we broadcast very loudly
            None
        } else {
            Some(Web3Connections::try_new(private_rpcs, Some(http_client), &clock).await?)
        };

        Ok(Web3ProxyApp {
            clock,
            balanced_rpc_tiers,
            private_rpcs,
            response_cache: Default::default(),
        })
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        json_body: JsonRpcRequest,
    ) -> anyhow::Result<impl warp::Reply> {
        if self.private_rpcs.is_some() && json_body.method == "eth_sendRawTransaction" {
            let private_rpcs = self.private_rpcs.as_ref().unwrap();

            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            loop {
                // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
                match private_rpcs.get_upstream_servers() {
                    Ok(upstream_servers) => {
                        let (tx, rx) = flume::unbounded();

                        let connections = private_rpcs.clone();
                        let method = json_body.method.clone();
                        let params = json_body.params.clone();

                        tokio::spawn(async move {
                            connections
                                .try_send_requests(upstream_servers, method, params, tx)
                                .await
                        });

                        // wait for the first response
                        let response = rx.recv_async().await?;

                        if let Ok(partial_response) = response {
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": json_body.id,
                                "result": partial_response
                            });
                            return Ok(warp::reply::json(&response));
                        }
                    }
                    Err(not_until) => {
                        // TODO: move this to a helper function
                        // sleep (with a lock) until our rate limits should be available
                        if let Some(not_until) = not_until {
                            let deadline = not_until.wait_time_from(self.clock.now());

                            sleep(deadline).await;
                        }
                    }
                };
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            // try to send to each tier, stopping at the first success
            loop {
                // there are multiple tiers. save the earliest not_until (if any). if we don't return, we will sleep until then and then try again
                let mut earliest_not_until = None;

                for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                    let current_block = balanced_rpcs.head_block_number(); // TODO: we don't store current block for everything anymore. we store it on the connections

                    // TODO: building this cache key is slow and its large, but i don't see a better way right now
                    // TODO: inspect the params and see if a block is specified. if so, use that block number instead of current_block
                    let cache_key = (
                        current_block,
                        json_body.method.clone(),
                        json_body.params.to_string(),
                    );

                    if let Some(cached) = self.response_cache.read().await.get(&cache_key) {
                        // TODO: this still serializes every time
                        return Ok(warp::reply::json(cached));
                    }

                    // TODO: what allowed lag?
                    match balanced_rpcs.next_upstream_server().await {
                        Ok(upstream_server) => {
                            let response = balanced_rpcs
                                .try_send_request(
                                    &upstream_server,
                                    &json_body.method,
                                    &json_body.params,
                                )
                                .await;

                            let response = match response {
                                Ok(partial_response) => {
                                    // TODO: trace here was really slow with millions of requests.
                                    // info!("forwarding request from {}", upstream_server);

                                    let response = json!({
                                        // TODO: re-use their jsonrpc?
                                        "jsonrpc": "2.0",
                                        "id": json_body.id,
                                        // TODO: since we only use the result here, should that be all we return from try_send_request?
                                        "result": partial_response.result,
                                    });

                                    // TODO: small race condidition here. parallel requests with the same query will both be saved to the cache
                                    let mut response_cache = self.response_cache.write().await;

                                    response_cache.insert(cache_key, response.clone());
                                    if response_cache.len() >= RESPONSE_CACHE_CAP {
                                        response_cache.pop_front();
                                    }

                                    response
                                }
                                Err(e) => {
                                    // TODO: what is the proper format for an error?
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": json_body.id,
                                        "error": format!("{}", e)
                                    })
                                }
                            };

                            return Ok(warp::reply::json(&response));
                        }
                        Err(None) => {
                            // TODO: this is too verbose. if there are other servers in other tiers, use those!
                            // warn!("No servers in sync!");
                        }
                        Err(Some(not_until)) => {
                            // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                            // TODO: helper function for this
                            if earliest_not_until.is_none() {
                                earliest_not_until.replace(not_until);
                            } else {
                                let earliest_possible =
                                    earliest_not_until.as_ref().unwrap().earliest_possible();

                                let new_earliest_possible = not_until.earliest_possible();

                                if earliest_possible > new_earliest_possible {
                                    earliest_not_until = Some(not_until);
                                }
                            }
                        }
                    }
                }

                // we haven't returned an Ok, sleep and try again
                if let Some(earliest_not_until) = earliest_not_until {
                    let deadline = earliest_not_until.wait_time_from(self.clock.now());

                    sleep(deadline).await;
                } else {
                    // TODO: how long should we wait?
                    // TODO: max wait time?
                    sleep(Duration::from_millis(500)).await;
                };
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt::init();

    let cli_config: CliConfig = argh::from_env();

    info!("Loading rpc config @ {}", cli_config.rpc_config_path);
    let rpc_config: String = fs::read_to_string(cli_config.rpc_config_path)?;
    let rpc_config: RpcConfig = toml::from_str(&rpc_config)?;

    // TODO: load the config from yaml instead of hard coding
    // TODO: support multiple chains in one process? then we could just point "chain.stytt.com" at this and caddy wouldn't need anything else
    // TODO: be smart about about using archive nodes? have a set that doesn't use archive nodes since queries to them are more valuable
    let listen_port = cli_config.listen_port;

    let app = rpc_config.try_build().await?;

    let app: Arc<Web3ProxyApp> = Arc::new(app);

    let proxy_rpc_filter = warp::any()
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| app.clone().proxy_web3_rpc(json_body));

    // TODO: filter for displaying connections and their block heights

    // TODO: warp trace is super verbose. how do we make this more readable?
    // let routes = proxy_rpc_filter.with(warp::trace::request());
    let routes = proxy_rpc_filter.map(handle_anyhow_errors);

    warp::serve(routes).run(([0, 0, 0, 0], listen_port)).await;

    Ok(())
}

/// convert result into an http response. use this at the end of your warp filter
/// TODO: using boxes can't be the best way. think about this more
fn handle_anyhow_errors<T: warp::Reply>(
    res: anyhow::Result<T>,
) -> warp::http::Response<warp::hyper::Body> {
    match res {
        Ok(r) => r.into_response(),
        Err(e) => warp::reply::with_status(
            format!("{}", e),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response(),
    }
}