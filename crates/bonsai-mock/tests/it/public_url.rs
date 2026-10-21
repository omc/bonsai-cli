//! `MockOptions::public_url` and `checkout_url`: what the demo's `--public-url`
//! and `--checkout-url` flags set.

use crate::common::create_body;
use bonsai_mock::{MockOptions, seed};
use serde_json::Value;

struct Served {
    h: bonsai_mock::MockHandle,
    db_path: std::path::PathBuf,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn serve(public_url: Option<&str>, checkout_url: Option<&str>) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mock.sqlite");
    let h = bonsai_mock::spawn(
        MockOptions {
            db_path: db_path.clone(),
            step_ms: 0,
            public_url: public_url.map(|u| u.parse().unwrap()),
            checkout_url: checkout_url.map(str::to_owned),
        },
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let http = bonsai_api::http_client().no_proxy().build().unwrap();
    Served {
        h,
        db_path,
        http,
        _dir: dir,
    }
}

impl Served {
    /// POST /v1/clusters on the bound address, with the given `Host`.
    async fn create(&self, token: &str, host: &str, plan: &str) -> reqwest::Response {
        self.http
            .post(format!("http://{}/v1/clusters", self.h.local_addr))
            .header("host", host)
            .header("idempotency-key", "k1")
            .bearer_auth(token)
            .json(&create_body("Demo", plan))
            .send()
            .await
            .unwrap()
    }

    fn token(&self, account: &str) -> String {
        self.h
            .seeded_tokens
            .iter()
            .find(|t| t.account_id == account)
            .unwrap()
            .token
            .clone()
    }

    fn globex_writer(&self) -> String {
        let conn = bonsai_mock::db::open(&self.db_path).unwrap();
        seed::create_token(
            &conn,
            "globex",
            "writer",
            &["account:read", "clusters:read", "clusters:write"],
            None,
        )
        .unwrap()
    }
}

#[tokio::test]
async fn a_public_url_host_is_served_and_used_for_links() {
    let m = serve(Some("https://api.bonsai.io"), None).await;
    assert_eq!(m.h.base_url.as_str(), "https://api.bonsai.io/v1");

    let res = m
        .create(&m.token("acme"), "api.bonsai.io", "standard-fir")
        .await;
    assert_eq!(res.status(), 202);
    let location = res.headers()["location"].to_str().unwrap();
    assert!(
        location.starts_with("https://api.bonsai.io/v1/actions/act_"),
        "{location}"
    );

    // Without the flag a foreign host is still refused.
    let m = serve(None, None).await;
    let res = m
        .create(&m.token("acme"), "api.bonsai.io", "standard-fir")
        .await;
    assert_eq!(res.status(), 421);
}

#[tokio::test]
async fn checkout_url_replaces_the_mock_checkout_page() {
    let prefix = "https://checkout.stripe.com/c/pay/";
    let m = serve(None, Some(prefix)).await;
    let token = m.globex_writer();
    let host = m.h.local_addr.to_string();

    let res = m.create(&token, &host, "standard-fir").await;
    assert_eq!(res.status(), 422);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["code"], "payment_method_required");
    let url = body["action_url"].as_str().unwrap();
    let id = url.strip_prefix(prefix).expect(url);
    let rest = id.strip_prefix("cs_test_").expect(id);
    assert_eq!(rest.len(), 24);
    assert!(
        rest.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    );

    // payment_failed takes the same link.
    let conn = bonsai_mock::db::open(&m.db_path).unwrap();
    bonsai_mock::scenario::set(&conn, "fail_next", &serde_json::json!("payment_failed")).unwrap();
    let res = m.create(&m.token("acme"), &host, "standard-fir").await;
    assert_eq!(res.status(), 422);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["code"], "payment_failed");
    assert!(
        body["action_url"]
            .as_str()
            .unwrap()
            .starts_with("https://checkout.stripe.com/c/pay/cs_test_")
    );
}
