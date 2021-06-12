#![allow(clippy::unwrap_used)]

use std::{collections::HashMap, sync::Arc};

use async_lock::Mutex;
use async_std::prelude::*;
use once_cell::sync::Lazy;
use openidconnect::{HttpRequest, HttpResponse};
use tide::{http::headers::LOCATION, Request, StatusCode};
use tide_testing::TideTestingExt;

use crate::{
    ClientId, ClientSecret, IssuerUrl, OpenIdConnectMiddleware, OpenIdConnectRouteExt, RedirectUrl,
};

const SECRET: [u8; 32] = *b"secrets must be >= 32 bytes long";

static ISSUER_URL: Lazy<IssuerUrl> =
    Lazy::new(|| IssuerUrl::new("https://localhost/issuer_url".to_string()).unwrap());
static CLIENT_ID: Lazy<ClientId> = Lazy::new(|| ClientId::new("CLIENT-ID".to_string()));
static CLIENT_SECRET: Lazy<ClientSecret> =
    Lazy::new(|| ClientSecret::new("CLIENT-SECRET".to_string()));
static REDIRECT_URL: Lazy<RedirectUrl> =
    Lazy::new(|| RedirectUrl::new("https://localhost/callback".to_string()).unwrap());

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum Error {
    // /// Test error.
// #[error("Test error: {}", _0)]
// Test(String),
}

type PendingResponse = (String, Result<HttpResponse, Error>);

task_local! {
    static PENDING_RESPONSE: Arc<Mutex<Vec<PendingResponse>>> =
        Arc::new(Mutex::new(vec![]));
}

async fn set_pending_response(response: Vec<PendingResponse>) {
    let pending_response_guard = PENDING_RESPONSE.with(|pr| pr.clone());
    let mut pending_response = pending_response_guard.lock().await;
    *pending_response = response;
}

async fn pending_response_is_empty() -> bool {
    let pending_response_guard = PENDING_RESPONSE.with(|pr| pr.clone());
    let pending_response = pending_response_guard.lock().await;
    pending_response.is_empty()
}

pub(crate) async fn http_client(openid_request: HttpRequest) -> Result<HttpResponse, Error> {
    // Get the pending response, which must exist (otherwise the test
    // has a bug).
    let pending_response_guard = PENDING_RESPONSE.with(|pr| pr.clone());
    let mut pending_response = pending_response_guard.lock().await;

    // Pop the first request from the vector, *ensure that it matches
    // the request URI,* then return that response.
    if pending_response.is_empty() {
        panic!("No pending response for URL \"{}\"", openid_request.url);
    }
    let (expected_uri, response) = pending_response.remove(0);
    assert_eq!(openid_request.url.to_string(), expected_uri);
    response
}

fn create_discovery_response() -> PendingResponse {
    (
        "https://localhost/issuer_url/.well-known/openid-configuration".to_string(),
        Ok(HttpResponse {
            status_code: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: "{
                \"issuer\":\"https://localhost/issuer_url\",
                \"authorization_endpoint\":\"https://localhost/authorization\",
                \"jwks_uri\":\"https://localhost/jwks\",
                \"response_types_supported\":[\"code\"],
                \"subject_types_supported\":[\"public\"],
                \"id_token_signing_alg_values_supported\":[\"RS256\"]
            }"
            .as_bytes()
            .into(),
        }),
    )
}

fn create_jwks_response() -> PendingResponse {
    (
        "https://localhost/jwks".to_string(),
        Ok(HttpResponse {
            status_code: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: "{\"keys\":[]}".as_bytes().into(),
        }),
    )
}

#[async_std::test]
async fn unauthed_request_redirects_to_login_uri() -> tide::Result<()> {
    let mut app = tide::new();
    app.with(tide::sessions::SessionMiddleware::new(
        tide::sessions::MemoryStore::new(),
        &SECRET,
    ));

    set_pending_response(vec![create_discovery_response(), create_jwks_response()]).await;

    app.with(
        OpenIdConnectMiddleware::new(&ISSUER_URL, &CLIENT_ID, &CLIENT_SECRET, &REDIRECT_URL).await,
    );

    app.at("/")
        .authenticated()
        .get(|_req: Request<()>| -> std::pin::Pin<Box<dyn Future<Output = tide::Result> + Send>> {
            panic!(
                "An unauthenticated request should not have made it to an `authenticated()` handler."
            );
        });

    let res = app.get("/").await?;
    assert_eq!(res.status(), StatusCode::Found);
    assert_eq!(
        res.header(LOCATION).unwrap().get(0).unwrap().to_string(),
        "/login"
    );

    Ok(())
}

#[async_std::test]
async fn middleware_can_be_initialized() -> tide::Result<()> {
    set_pending_response(vec![create_discovery_response(), create_jwks_response()]).await;

    OpenIdConnectMiddleware::new(&ISSUER_URL, &CLIENT_ID, &CLIENT_SECRET, &REDIRECT_URL).await;

    assert!(pending_response_is_empty().await);

    Ok(())
}

#[async_std::test]
async fn middleware_implements_login_handler() -> tide::Result<()> {
    let mut app = tide::new();
    app.with(tide::sessions::SessionMiddleware::new(
        tide::sessions::MemoryStore::new(),
        &SECRET,
    ));

    set_pending_response(vec![create_discovery_response(), create_jwks_response()]).await;

    app.with(
        OpenIdConnectMiddleware::new(&ISSUER_URL, &CLIENT_ID, &CLIENT_SECRET, &REDIRECT_URL).await,
    );

    let res = app.get("/login").await?;
    assert_eq!(res.status(), StatusCode::Found);

    let url =
        openidconnect::url::Url::parse(res.header(LOCATION).unwrap().get(0).unwrap().as_str())
            .unwrap();
    assert_eq!(url.host_str().unwrap(), "localhost");
    assert_eq!(url.path(), "/authorization");
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query.get("response_type").unwrap(), "code");
    assert_eq!(query.get("client_id").unwrap(), CLIENT_ID.as_str());
    assert_eq!(query.get("scope").unwrap(), "openid");
    assert!(query.contains_key("state"));
    assert!(query.contains_key("nonce"));
    assert_eq!(
        query.get("redirect_uri").unwrap(),
        "https://localhost/callback"
    );

    Ok(())
}

// async fn login_path_can_be_changed() -> tide::Result<()> {
// Same as above, but changing the /login path works.

// async fn oauth_scopes_can_be_changed() -> tide::Result<()> {
// Same as above, but now the new/different scopes show up in the authorize_url.

// async fn logic_panics_on_missing_session_middleware() -> tide::Result<()> {
// Same as above, but we get a panic if the session middleware was not configured.

// async fn middleware_implements_redirect_handler() -> tide::Result<()> {
// Request to redirect_url (with the authorization code and stuff): checks the nonce and CSRF, makes the token call, sets session state, can get req.user_id() or whatever.

// async fn redirect_handler_rejects_invalid_csrf() -> tide::Result<()> {
// Same as above but with a non-matching CSRF: error.

// async fn redirect_handler_rejects_invalid_nonce() -> tide::Result<()> {
// Same as above but with a non-matching nonce: error.

// async fn redirect_handler_errors_on_missing_session_middleware() -> tide::Result<()> {
// *Error* (not panic) on missing session middleware, since this is indistinguishable from an expired session that was simply not present in the session store.
// I *think.* Let's verify that this is in fact what happens, because maybe we want one version that panics (if we can in fact detect that the session middleware is missing).

// TODO Move these to `route_ext.rs`?
// async fn unauthenticated_routes_do_not_force_login() -> tide::Result<()> {
// Basically: a request to a random /foo URL works.

// async fn authenticated_routes_require_login() -> tide::Result<()> {
// Basically: a request to a an `.authenticated().` /foo URL redirects to /login.

// async fn authenticated_and_unauthenticated_routes_can_coexist() -> tide::Result<()> {
// Basically: two routes, one that works and one that redirects to /login.
