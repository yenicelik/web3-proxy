mod app;
mod config;
mod connection;
mod connections;
mod jsonrpc;

use jsonrpc::{JsonRpcErrorData, JsonRpcForwardedResponse};
use parking_lot::deadlock;
use serde_json::value::RawValue;
use std::env;
use std::fs;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime;
use tracing::{info, warn};
use warp::Filter;
use warp::Reply;

use crate::app::Web3ProxyApp;
use crate::config::{CliConfig, RpcConfig};

fn main() -> anyhow::Result<()> {
    // TODO: is there a better way to do this?
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "web3_proxy=debug");
    }

    // install global collector configured based on RUST_LOG env var.
    console_subscriber::init();

    fdlimit::raise_fd_limit();

    let cli_config: CliConfig = argh::from_env();

    info!("Loading rpc config @ {}", cli_config.config);
    let rpc_config: String = fs::read_to_string(cli_config.config)?;
    let rpc_config: RpcConfig = toml::from_str(&rpc_config)?;

    // TODO: this doesn't seem to do anything
    proctitle::set_title(format!("web3-proxy-{}", rpc_config.shared.chain_id));

    let chain_id = rpc_config.shared.chain_id;

    // TODO: multithreaded runtime once i'm done debugging
    let mut rt_builder = runtime::Builder::new_current_thread();

    rt_builder.enable_all().thread_name_fn(move || {
        static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
        // TODO: what ordering? i think we want seqcst so that these all happen in order, but that might be stricter than we really need
        let worker_id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
        // TODO: i think these max at 15 characters
        format!("web3-{}-{}", chain_id, worker_id)
    });

    if cli_config.workers > 0 {
        rt_builder.worker_threads(cli_config.workers);
    }

    let rt = rt_builder.build()?;

    // spawn a thread for deadlock detection
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }

        println!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            println!("Deadlock #{}", i);
            for t in threads {
                println!("Thread Id {:#?}", t.thread_id());
                println!("{:#?}", t.backtrace());
            }
        }
    });

    // spawn the root task
    rt.block_on(async {
        let listen_port = cli_config.port;

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
    })
}

/// convert result into a jsonrpc error. use this at the end of your warp filter
fn handle_anyhow_errors<T: warp::Reply>(
    res: anyhow::Result<T>,
) -> warp::http::Response<warp::hyper::Body> {
    match res {
        Ok(r) => r.into_response(),
        Err(e) => {
            warn!("Responding with error: {:?}", e);

            let e = JsonRpcForwardedResponse {
                jsonrpc: "2.0".to_string(),
                // TODO: what id can we use? how do we make sure the incoming id gets attached to this?
                id: RawValue::from_string("0".to_string()).unwrap(),
                result: None,
                error: Some(JsonRpcErrorData {
                    code: -32099,
                    message: format!("{:?}", e),
                    data: None,
                }),
            };

            warp::reply::with_status(
                serde_json::to_string(&e).unwrap(),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
        .into_response(),
    }
}
