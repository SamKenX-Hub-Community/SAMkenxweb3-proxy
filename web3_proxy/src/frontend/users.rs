// So the API needs to show for any given user:
// - show balance in USD
// - show deposits history (currency, amounts, transaction id)
// - show number of requests used (so we can calculate average spending over a month, burn rate for a user etc, something like "Your balance will be depleted in xx days)
// - the email address of a user if he opted in to get contacted via email
// - all the monitoring and stats but that will come from someplace else if I understand corectly?
// I wonder how we handle payment
// probably have to do manual withdrawals

use super::{
    errors::{anyhow_error_into_response, FrontendResult},
    rate_limit::RateLimitResult,
};
use crate::app::Web3ProxyApp;
use axum::{
    extract::Path,
    response::{IntoResponse, Response},
    Extension, Json,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use entities::user;
use ethers::{prelude::Address, types::Bytes};
use redis_rate_limit::redis::{pipe, AsyncCommands};
use reqwest::StatusCode;
use sea_orm::ActiveModelTrait;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

// TODO: how do we customize axum's error response? I think we probably want an enum that implements IntoResponse instead
#[debug_handler]
pub async fn get_login(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    // TODO: what does axum's error handling look like?
    Path(user_address): Path<Address>,
) -> FrontendResult {
    // TODO: refactor this to use the try operator
    let _ip = match app.rate_limit_by_ip(ip).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedIp(x)) => x,
            Err(err_response) => return Ok(err_response),
            _ => unimplemented!(),
        },
        Err(err) => return Ok(anyhow_error_into_response(None, None, err)),
    };

    // at first i thought about checking that user_address is in our db
    // but theres no need to separate the create_user and login flows
    // its a better UX to just click "login with ethereum" and have the account created if it doesn't exist
    // we can prompt for an email and and payment after they log in

    let session_id = uuid::Uuid::new_v4();

    // TODO: if no redis, store in local cache?
    let redis_pool = app
        .redis_pool
        .as_ref()
        .expect("login requires a redis server");

    let mut redis_conn = redis_pool.get().await.unwrap();

    // TODO: how many seconds? get from config?
    let session_expiration_seconds = 300;

    let reply: String = redis_conn
        .set_ex(
            session_id.to_string(),
            user_address.to_string(),
            session_expiration_seconds,
        )
        .await
        .unwrap();

    todo!("how should this work? probably keep stuff in redis")
}

#[debug_handler]
pub async fn create_user(
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(payload): Json<CreateUser>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> Response {
    // TODO: return a Result instead
    let _ip = match app.rate_limit_by_ip(ip).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedIp(x)) => x,
            Err(err_response) => return err_response,
            _ => unimplemented!(),
        },
        Err(err) => return anyhow_error_into_response(None, None, err),
    };

    // TODO: check invite_code against the app's config or database
    if payload.invite_code != "llam4n0des!" {
        todo!("proper error message")
    }

    // TODO: dont unwrap. proper error
    let signature: [u8; 65] = payload.signature.as_ref().try_into().unwrap();

    // TODO: calculate the expected message for the current user. include domain and a nonce. let timestamp be automatic
    let message: siwe::Message = "abc123".parse().unwrap();
    if let Err(e) = message.verify(signature, None, None, None) {
        // message cannot be correctly authenticated
        todo!("proper error message: {}", e)
    }

    let user = user::ActiveModel {
        address: sea_orm::Set(payload.address.to_fixed_bytes().into()),
        email: sea_orm::Set(payload.email),
        ..Default::default()
    };

    let db = app.db_conn.as_ref().unwrap();

    // TODO: proper error message
    let user = user.insert(db).await.unwrap();

    // TODO: create

    // TODO: do not expose user ids
    (StatusCode::CREATED, Json(user)).into_response()
}

// the input to our `create_user` handler
#[derive(Deserialize)]
pub struct CreateUser {
    address: Address,
    // TODO: make sure the email address is valid
    email: Option<String>,
    signature: Bytes,
    invite_code: String,
}
