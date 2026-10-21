use bonsai_api::is_loopback_host;
use url::Url;

const CHECKOUT_HOST: &str = "checkout.stripe.com";

fn host_str(url: &Url) -> Option<String> {
    url.host_str().map(|h| h.to_ascii_lowercase())
}

/// Whether the CLI may open `url` in a browser: https (or http on
/// loopback) and a host that is Stripe Checkout, the API's or the dashboard's.
/// URLs with userinfo are refused.
pub fn may_open(url: &Url, api_url: &Url, dashboard_url: Option<&Url>) -> bool {
    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => is_loopback_host(url),
        _ => false,
    };
    if !scheme_ok || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = host_str(url) else {
        return false;
    };
    host == CHECKOUT_HOST
        || host_str(api_url).is_some_and(|h| h == host)
        || dashboard_url.and_then(host_str).is_some_and(|h| h == host)
}
