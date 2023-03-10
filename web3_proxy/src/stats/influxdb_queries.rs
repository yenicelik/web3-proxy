use super::StatType;
use crate::{
    app::Web3ProxyApp,
    frontend::errors::FrontendErrorResponse,
    http_params::{
        get_chain_id_from_params, get_query_start_from_params, get_query_stop_from_params,
        get_query_window_seconds_from_params, get_user_id_from_params,
    },
};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::{IntoResponse, Response},
    Json, TypedHeader,
};
use chrono::{DateTime, FixedOffset};
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::models::Query;
use influxdb2::FromDataPoint;
use itertools::Itertools;
use log::info;
use serde::Serialize;
use serde_json::{json};

// TODO: include chain_id, method, and some other things in this struct
#[derive(Debug, Default, FromDataPoint, Serialize)]
pub struct AggregatedRpcAccounting {
    field: String,
    value: f64,
    time: DateTime<FixedOffset>,
}

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
) -> Result<Response, FrontendErrorResponse> {
    info!("Got this far 1");
    let db_conn = app.db_conn().context("query_user_stats needs a db")?;
    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;
    info!("Got this far 2");
    let mut redis_conn = app
        .redis_conn()
        .await
        .context("query_user_stats had a redis connection error")?
        .context("query_user_stats needs a redis")?;

    // TODO: have a getter for this. do we need a connection pool on it?
    info!("Got this far 3");
    let influxdb_client = app
        .influxdb_client
        .as_ref()
        .context("query_user_stats needs an influxdb client")?;

    info!("Got this far 4");
    // get the user id first. if it is 0, we should use a cache on the app
    let user_id =
        get_user_id_from_params(&mut redis_conn, &db_conn, &db_replica, bearer, params).await?;

    info!("Got this far 5");
    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?.timestamp();
    let query_stop = get_query_stop_from_params(params)?.timestamp();
    let chain_id = get_chain_id_from_params(app, params)?;

    // query_window_seconds must be provided, and should be not 1s (?) by default ..

    // Return a bad request if query_start == query_stop, because then the query is empty basically
    if query_start == query_stop {
        return Err(FrontendErrorResponse::BadRequest("Start and Stop date cannot be equal. Please specify a (different) start date.".to_owned()));
    }

    info!("Got this far 6");
    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    // from(bucket: "dev_web3_proxy")
    //     |> range(start: v.timeRangeStart, stop: v.timeRangeStop)
    //     |> filter(fn: (r) => r["_measurement"] == "opt_in_proxy" or r["_measurement"] == "global_proxy")
    // |> filter(fn: (r) => r["_field"] == "frontend_requests" or r["_field"] == "backend_requests" or r["_field"] == "sum_request_bytes")
    // |> group(columns: ["_field", "_measurement"])
    //     |> aggregateWindow(every: v.windowPeriod, fn: mean, createEmpty: false)
    // |> yield(name: "mean")

    // TODO: Should be taken from the config, not hardcoded ...
    // TODO: Turn into a 500 error if bucket is not found ..
    // Or just unwrap or so
    let bucket = &app.config.influxdb_bucket.clone().context("No influxdb bucket was provided")?;  // "web3_proxy";
    info!("Bucket is {:?}", bucket);

    info!("Got this far 7");
    let mut group_columns = vec!["_measurement", "_field"];
    let mut filter_chain_id = "".to_string();

    if chain_id == 0 {
        group_columns.push("chain_id");
    } else {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
    }

    info!("Got this far 8");
    let group_columns = serde_json::to_string(&json!(group_columns)).unwrap();

    info!("Got this far 9");
    let group = match stat_response_type {
        StatType::Aggregated => f!(r#"|> group(columns: {group_columns})"#),
        StatType::Detailed => "".to_string(),
    };

    info!("Got this far 10");
    let filter_field = match stat_response_type {
        // StatType::Aggregated => f!(r#"|> filter(fn: (r) => r["_field"] == "frontend_requests")"#),
        // Let's show all endpoints in a detailed stats
        // StatType::Aggregated => "".to_string(),  // f!(r#"|> filter(fn: (r) => r["_field"] == "frontend_requests")"#),
        StatType::Aggregated => f!(r#"|> filter(fn: (r) => r["_field"] == "frontend_requests" or r["_field"] == "backend_requests" or r["_field"] == "cache_hits" or r["_field"] == "cache_misses" or r["_field"] == "no_servers" or r["_field"] == "sum_request_bytes" or r["_field"] == "sum_response_bytes" or r["_field"] == "sum_response_millis")"#),
        StatType::Detailed => "".to_string(),
    };

    info!("Query start and stop are: {:?} {:?}", query_start, query_stop);
    info!("Query measurement is: {:?}", measurement);
    info!("Filters are: {:?} {:?}", filter_field, filter_chain_id);
    info!("Group is: {:?}", group);
    info!("window seconds are: {:?}", query_window_seconds);

    // These are taken care of probably ...
    // reg. fields, collect: backend_requests, frontend_requests, cache_hits, cache_misses, total_request_bytes, total_response_bytes, total_response_millis
    // "total_frontend_requests": "6",
    // "total_response_bytes": "235",
    // "total_response_millis": "0"
    // "total_cache_hits": "6",
    // "total_cache_misses": "0",

    // Perhaps gotta run a second query to get all error responses
    // "total_error_responses": "0",
    // Same with archive requests
    // "archive_request": 0,

    // Group by method if detailed, else just keep all methods as "null". i think influxdb takes care of that
    // "method": null,
    // "total_backend_retries": "0",

    info!("Got this far 11");
    let query = f!(r#"
        from(bucket: "{bucket}")
            |> range(start: {query_start}, stop: {query_stop})
            |> filter(fn: (r) => r["_measurement"] == "{measurement}")
            {filter_field}
            {filter_chain_id}
            {group}
            |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
            |> yield(name: "sum")
    "#);

    info!("Raw query to db is: {:?}", query);
    let query = Query::new(query.to_string());
    info!("Query to db is: {:?}", query);

    // TODO: do not unwrap. add this error to FrontErrorResponse
    // TODO: StatType::Aggregated and StatType::Detailed might need different types
    // let unparsed: serde_json::Value = serde_json::Value::Array(influxdb_client.query(Some(query.clone())).await?);
    // info!("Direct response is: {:?}", unparsed);
    info!("Got this far 12");

    let influx_responses: Vec<AggregatedRpcAccounting> = influxdb_client.query(Some(query)).await?;
    info!("Influx responses are {:?}", &influx_responses);
    for res in &influx_responses {
        info!("Resp is: {:?}", res);
    }

    // Group by all fields together ..
    let datapoints = influx_responses
        .into_iter()
        .group_by(|x| {
            // This looks ugly, revisit later
            // x.field.clone()
            (x.clone().time)
        })
        .into_iter()
        .map(|(group, grouped_items)| {
            // Now put all the fields next to each other
            // (there will be exactly one field per timestamp, but we want to arrive at a new object)
            let mut out = HashMap::new();
            // Could also add a timestamp

            for x in grouped_items {
                out.insert(
                    f!(r#"total_{x.field}"#),
                    // serde_json::Value::Number(serde_json::Number::from(x.value))
                    json!(x.value)
                );

                if !out.contains_key("query_window_timestamp") {
                    out.insert(
                        "query_window_timestamp".to_owned(),
                        // serde_json::Value::Number(x.time.timestamp().into())
                        json!(x.time.timestamp())
                    );
                }
            }
            json!(out)
        }).collect::<Vec<_>>();

    // I suppose archive requests could be either gathered by default (then summed up), or retrieved on a second go.
    // Same with error responses ..
    let mut out = HashMap::new();
    out.insert("num_items", serde_json::Value::Number(datapoints.len().into()));
    out.insert("result", serde_json::Value::Array(datapoints));
    out.insert("query_window_seconds", serde_json::Value::Number(query_window_seconds.into()));
    out.insert("query_start", serde_json::Value::Number(query_start.into()));
    out.insert("chain_id", serde_json::Value::Number(chain_id.into()));

    info!("Got this far 13 {:?}", out);
    let out = Json(json!(out)).into_response();
    // Add the requests back into out

    info!("Got this far 14 {:?}", out);


    // TODO: Now impplement the proper response type

    Ok(out)
}
