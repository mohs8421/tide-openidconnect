// #![forbid(unsafe_code, future_incompatible)]
// #![deny(
//     missing_debug_implementations,
//     nonstandard_style,
//     missing_docs,
//     unreachable_pub,
//     missing_copy_implementations,
//     unused_qualifications
// )]

use openidconnect::{
    core::{CoreClient, CoreProviderMetadata, CoreResponseType},
    reqwest::http_client,
    AuthenticationFlow, AuthorizationCode, CsrfToken, Nonce, OAuth2TokenResponse,
};
use serde::Deserialize;
use tide::{
    http::cookies::SameSite,
    http::{Cookie, Method},
    Middleware, Next, Request, Response, StatusCode,
};

pub use openidconnect::{ClientId, ClientSecret, IssuerUrl, RedirectUrl};

#[derive(Debug, Deserialize)]
struct OpenIdCallback {
    pub code: AuthorizationCode,
    pub state: String,
}

struct OpenIdConnectRequestExtData {
    is_authenticated: bool,
    user_id: String,
}

pub trait OpenIdConnectRequestExt {
    fn is_authenticated(&self) -> bool;
    fn user_id(&self) -> &str;
}

impl<State> OpenIdConnectRequestExt for Request<State>
where
    State: Send + Sync + 'static,
{
    fn is_authenticated(&self) -> bool {
        let ext_data: &OpenIdConnectRequestExtData = self
            .ext()
            .expect("You must install OpenIdConnectMiddleware to access the Open ID request data.");
        ext_data.is_authenticated
    }

    fn user_id(&self) -> &str {
        let ext_data: &OpenIdConnectRequestExtData = self
            .ext()
            .expect("You must install OpenIdConnectMiddleware to access the Open ID request data.");
        &ext_data.user_id
    }
}

/// # Middleware to enable OpenID Connect-based authentication
///
/// ... add docs ...
///
/// ## Example
/// ```rust
/// use tide_csrf::{self, CsrfRequestExt};
///
/// # async_std::task::block_on(async {
/// let mut app = tide::new();
///
/// app.with(tide_csrf::CsrfMiddleware::new(
///     b"we recommend you use std::env::var(\"TIDE_SECRET\").unwrap().as_bytes() instead of a fixed value"
/// ));
///
/// app.at("/").get(|req: tide::Request<()>| async move {
///     Ok(format!(
///         "CSRF token is {}; you should put that in header {}",
///         req.csrf_token(),
///         req.csrf_header_name()
///     ))
/// });
///
/// # })
/// ```

pub struct OpenIdConnectMiddleware {
    login_path: String,
    redirect_url: RedirectUrl,
    landing_path: String,
    client: CoreClient,
}

impl std::fmt::Debug for OpenIdConnectMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenIdConnectMiddleware")
            .field("login_path", &self.login_path)
            .field("redirect_url", &self.redirect_url)
            .field("landing_path", &self.landing_path)
            .finish()
    }
}

impl OpenIdConnectMiddleware {
    pub async fn new(
        issuer_url: IssuerUrl,
        client_id: ClientId,
        client_secret: ClientSecret,
        redirect_url: RedirectUrl,
    ) -> Self {
        // Get the OpenID Connect provider metadata.
        let provider_metadata = CoreProviderMetadata::discover(&issuer_url, http_client).unwrap();

        // Create the OpenID Connect client.
        let client =
            CoreClient::from_provider_metadata(provider_metadata, client_id, Some(client_secret))
                .set_redirect_uri(redirect_url.clone());

        // Initialize the middleware with our defaults.
        Self {
            login_path: "/login".to_string(),
            redirect_url,
            landing_path: "/".to_string(),
            client,
        }
    }

    /// Sets the path to the "login" route that will be intercepted by the
    /// middleware in order to redirect the browser to the OpenID Connect
    /// authentication page.
    ///
    /// Defaults to "/login".
    pub fn with_login_path(mut self, login_path: &str) -> Self {
        self.login_path = login_path.to_string();
        self
    }

    /// Sets the path where the browser will be sent after a successful
    /// login sequence.
    ///
    /// Defaults to "/".
    pub fn with_landing_path(mut self, landing_path: &str) -> Self {
        self.landing_path = landing_path.to_string();
        self
    }

    async fn generate_redirect<State>(&self, req: Request<State>) -> tide::Result
    where
        State: Clone + Send + Sync + 'static,
    {
        let (authorize_url, csrf_state, nonce) = self
            .client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            // TODO Scopes will need to be configurable once we turn this into middleware.
            // FIXME Crashes if we enable this due to: <https://github.com/ramosbugs/openidconnect-rs/issues/23>
            // .add_scope(Scope::new("profile".to_string()))
            .url();

        let mut response = Response::builder(StatusCode::Found)
            .header(tide::http::headers::LOCATION, authorize_url.to_string())
            .build();

        // TODO These cookies may not work in all cases (if you navigate
        // directly to the site) given that they are trying to set a path
        // of `/`, but the URLs are located below that... Need to see what
        // the Auth0 Express.js thing does with these cookies.
        // TODO Is it that we want SameSite::Lax instead of Strict? I think
        // that is the case... (but let's test/confirm)
        let openid_csrf_cookie = Cookie::build("tide.openid_csrf", csrf_state.secret().clone())
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .secure(req.url().scheme() == "https")
            .finish();
        response.insert_cookie(openid_csrf_cookie);

        let openid_nonce_cookie = Cookie::build("tide.openid_nonce", nonce.secret().clone())
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .secure(req.url().scheme() == "https")
            .finish();
        response.insert_cookie(openid_nonce_cookie);

        Ok(response)
    }

    async fn handle_callback<State>(&self, req: Request<State>) -> tide::Result
    where
        State: Clone + Send + Sync + 'static,
    {
        // Get the auth CSRF and Nonce values from the cookies.
        let _openid_csrf_cookie = req.cookie("tide.openid_csrf").unwrap();

        let openid_nonce_cookie = req.cookie("tide.openid_nonce").unwrap();
        let nonce = Nonce::new(openid_nonce_cookie.value().to_string());

        // Extract the OpenID callback information and verify the CSRF
        // cookie.
        let callback_data: OpenIdCallback = req.query()?;
        // TODO Verify state against `tide.openid_csrf` cookie.

        // Exchange the code for a token.
        // TODO Needs to use an async HTTP client, which means we need to
        // build an openidconnect adapter to surf (which uses async-std,
        // just like Tide).
        let token_response = self
            .client
            .exchange_code(callback_data.code)
            .request(http_client)
            .unwrap();
        println!("Access token: {}", token_response.access_token().secret());
        println!("Scopes: {:?}", token_response.scopes());

        // Get the claims and verify the nonce.
        let claims = token_response
            .extra_fields()
            .id_token()
            .expect("Server did not return an ID token")
            .claims(&self.client.id_token_verifier(), &nonce)
            .unwrap();
        println!("ID token: {:?}", claims);
        println!("User id: {}", claims.subject().as_str());

        // The user has logged in; redirect them to the main site.
        let mut response = Response::builder(StatusCode::Found)
            .header(tide::http::headers::LOCATION, &self.landing_path)
            .build();

        let openid_csrf_cookie = Cookie::build("tide.openid_csrf", "")
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .secure(req.url().scheme() == "https")
            .finish();
        response.remove_cookie(openid_csrf_cookie);

        let openid_nonce_cookie = Cookie::build("tide.openid_nonce", "")
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .secure(req.url().scheme() == "https")
            .finish();
        response.remove_cookie(openid_nonce_cookie);

        Ok(response)
    }
}

#[tide::utils::async_trait]
impl<State> Middleware<State> for OpenIdConnectMiddleware
where
    State: Clone + Send + Sync + 'static,
{
    async fn handle(&self, req: Request<State>, next: Next<'_, State>) -> tide::Result {
        // Is this URL one of the URLs that we need to intercept as part
        // of the OpenID Connect auth process? If so, apply the appropriate
        // part of the auth process according to the URL. If not, verify
        // that the request is authenticated, and if not, redirect the
        // browser to the login URL. And if they are authenticated, then
        // just proceed to the handler (after populating the request extension
        // fields).
        if req.method() == Method::Get && req.url().path() == self.login_path {
            self.generate_redirect(req).await
        } else if req.method() == Method::Get && req.url().path() == self.redirect_url.url().path()
        {
            self.handle_callback(req).await
        } else {
            // TODO Need a check to see if we are authenticated (req.session() has our data).

            // Request is authenticated; add our extension data to the
            // request.

            // Call the downstream middleware.
            let response = next.run(req).await;

            // Return the response.
            Ok(response)
        }
    }
}

#[cfg(test)]
mod tests {
    // use super::*;
    // use tide::Request;
    // use tide_testing::{surf::Response, TideTestingExt};
}
