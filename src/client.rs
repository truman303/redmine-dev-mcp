use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Url, redirect};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{ConfigError, RedmineError};

const MAX_GET_RETRIES: u32 = 2;
const MAX_REDIRECTS: usize = 5;

#[derive(Clone)]
pub struct Client {
    config: Config,
    base: Url,
    http: reqwest::Client,
    /// Projects whose checklist endpoint answered 403 or 404. Per project,
    /// because the checklist plugin is a module each project turns on or off.
    /// Shared by clones.
    checklists_absent: Arc<Mutex<HashSet<u64>>>,
}

impl Client {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let base = base_url(&config.base_url)?;
        let mut key = HeaderValue::from_str(&config.api_key)
            .map_err(|_| ConfigError("REDMINE_API_KEY is not a valid header value".to_string()))?;
        // Keeps the key out of any debug output from reqwest and hyper.
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(HeaderName::from_static("x-redmine-api-key"), key);

        // reqwest strips Authorization and Cookie on a cross-host redirect, but not
        // custom headers. Without this policy, an SSO proxy that redirects API calls
        // to a login host would receive the API key.
        let redirect_base = base.clone();
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if same_instance(&redirect_base, attempt.url()) {
                attempt.follow()
            } else {
                let to = attempt.url().origin().ascii_serialization();
                attempt.error(format!(
                    "refused a redirect to {to}, which is not REDMINE_URL. Is REDMINE_URL the address the API answers on?"
                ))
            }
        });

        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .default_headers(headers)
            .redirect(policy)
            .user_agent(concat!("redmine-dev-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| ConfigError(format!("could not build HTTP client: {err}")))?;
        Ok(Self {
            config,
            base,
            http,
            checklists_absent: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn checklists_absent(&self, project_id: u64) -> bool {
        self.checklists_absent
            .lock()
            .map(|set| set.contains(&project_id))
            .unwrap_or(false)
    }

    pub fn mark_checklists_absent(&self, project_id: u64) {
        if let Ok(mut set) = self.checklists_absent.lock() {
            set.insert(project_id);
        }
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, RedmineError> {
        let (response, path) = self.send(Method::GET, path, query, None).await?;
        let text = read_text(response, &path).await?;
        // Every Redmine GET returns a JSON object. An empty body is a fault, and
        // treating it as `{}` would look like a real empty result.
        serde_json::from_str(&text).map_err(|err| RedmineError::Json {
            message: format!("Unexpected response from {path}: {err}"),
            path,
        })
    }

    /// GET a body as bytes, stopping once it passes `limit`. Checks Content-Length
    /// first, so an oversized attachment is usually refused before any of it arrives.
    pub async fn get_bytes_limited(&self, path: &str, limit: u64) -> Result<Vec<u8>, RedmineError> {
        let (mut response, path) = self.send(Method::GET, path, &[], None).await?;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(RedmineError::TooLarge { path, limit });
        }
        let mut body = Vec::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|err| transport(&path, "Reading the response failed", &err))?;
            let Some(chunk) = chunk else {
                return Ok(body);
            };
            if (body.len() + chunk.len()) as u64 > limit {
                return Err(RedmineError::TooLarge { path, limit });
            }
            body.extend_from_slice(&chunk);
        }
    }

    pub async fn put_json(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<String, RedmineError> {
        let bytes = serde_json::to_vec(body).map_err(|err| RedmineError::Json {
            path: path.to_string(),
            message: format!("could not encode JSON: {err}"),
        })?;
        let (response, path) = self.send(Method::PUT, path, &[], Some(bytes)).await?;
        read_text(response, &path).await
    }

    /// Sends the request, retrying a GET on 429. Returns a success response and the
    /// path to quote in errors. A non-success status becomes `RedmineError::Http`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(reqwest::Response, String), RedmineError> {
        if method != Method::GET && self.config.read_only {
            return Err(RedmineError::ReadOnly);
        }
        let url =
            build_url(&self.base, path, query).map_err(|message| RedmineError::Transport {
                path: path.to_string(),
                message,
            })?;
        let path = safe_path(&url);
        tracing::debug!(%method, %path, "redmine request");

        let mut attempt = 0u32;
        loop {
            let mut builder = self.http.request(method.clone(), url.clone());
            if let Some(bytes) = &body {
                builder = builder
                    .header(CONTENT_TYPE, "application/json")
                    .body(bytes.clone());
            }
            let response = builder
                .send()
                .await
                .map_err(|err| transport(&path, "Redmine request failed", &err))?;

            // Reads only. A write that hit the limiter may still have landed,
            // and a retry would post the note twice.
            let retry = response.status() == StatusCode::TOO_MANY_REQUESTS
                && method == Method::GET
                && attempt < MAX_GET_RETRIES;
            if !retry {
                return check_status(response, path).await;
            }
            let wait_ms = retry_after_ms(response.headers().get("retry-after"));
            tracing::debug!(wait_ms, %path, "redmine rate limited");
            drop(response);
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
    }
}

fn base_url(raw: &str) -> Result<Url, ConfigError> {
    let with_slash = if raw.ends_with('/') {
        raw.to_string()
    } else {
        format!("{raw}/")
    };
    Url::parse(&with_slash).map_err(|err| ConfigError(format!("invalid REDMINE_URL {raw}: {err}")))
}

/// Same host and port as REDMINE_URL, and never a downgrade from https to http.
/// An upgrade from http to https on the default ports is allowed.
fn same_host(base: &Url, url: &Url) -> bool {
    if !url
        .host_str()
        .is_some_and(|host| Some(host) == base.host_str())
    {
        return false;
    }
    match (base.scheme(), url.scheme()) {
        (a, b) if a == b => url.port_or_known_default() == base.port_or_known_default(),
        ("http", "https") => base.port().is_none() && url.port().is_none(),
        _ => false,
    }
}

/// `same_host`, and under the base path, so `/redmine` cannot reach `/other-app`.
fn same_instance(base: &Url, url: &Url) -> bool {
    same_host(base, url) && url.path().starts_with(base.path())
}

/// Join `path` onto the base URL without dropping a path prefix such as `/redmine`.
/// An absolute URL is accepted only on this instance, because the API key goes with it.
pub(crate) fn build_url(base: &Url, path: &str, query: &[(&str, &str)]) -> Result<Url, String> {
    let mut url = if path.contains("://") {
        let url = Url::parse(path).map_err(|err| format!("invalid URL {path}: {err}"))?;
        if !same_instance(base, &url) {
            return Err(format!(
                "Refusing to send the API key to {}, which is not REDMINE_URL.",
                url.origin().ascii_serialization()
            ));
        }
        url
    } else {
        base.join(path.trim_start_matches('/'))
            .map_err(|err| format!("invalid path {path}: {err}"))?
    };
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

fn safe_path(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

/// reqwest's Display stops at the top error, which for a refused redirect or a TLS
/// failure is the least useful part. Walk the chain.
fn transport(path: &str, context: &str, err: &reqwest::Error) -> RedmineError {
    let mut message = format!("{context}: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    RedmineError::Transport {
        path: path.to_string(),
        message,
    }
}

fn retry_after_ms(header: Option<&HeaderValue>) -> u64 {
    let Some(header) = header else {
        return 2_000;
    };
    let Ok(text) = header.to_str() else {
        return 2_000;
    };
    let Ok(seconds) = text.trim().parse::<f64>() else {
        return 2_000;
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return 2_000;
    }
    (seconds.min(10.0) * 1_000.0).round() as u64
}

async fn read_text(response: reqwest::Response, path: &str) -> Result<String, RedmineError> {
    response
        .text()
        .await
        .map_err(|err| transport(path, "Reading the response failed", &err))
}

async fn check_status(
    response: reqwest::Response,
    path: String,
) -> Result<(reqwest::Response, String), RedmineError> {
    let status = response.status();
    if status.is_success() {
        return Ok((response, path));
    }
    // The body only decorates the message, so a failed read here is not worth
    // reporting over the status itself.
    let body = response.text().await.unwrap_or_default();
    Err(RedmineError::Http {
        status: status.as_u16(),
        message: format_http_error(status.as_u16(), &path, &body),
        path,
    })
}

fn format_http_error(status: u16, path: &str, body: &str) -> String {
    let hint = match status {
        401 => "Redmine rejected the API key. Check REDMINE_API_KEY.".to_string(),
        403 => "The API key cannot access this resource.".to_string(),
        404 => "Not found, or the API key cannot see it.".to_string(),
        429 => "Redmine is rate limiting this API key. Wait a few seconds before the next call."
            .to_string(),
        other => format!("Redmine returned HTTP {other}."),
    };
    match error_snippet(body) {
        Some(snippet) => format!("{hint} ({path}: {snippet})"),
        None => format!("{hint} ({path})"),
    }
}

fn error_snippet(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(errors) = parsed.get("errors").and_then(|value| value.as_array())
    {
        let text = errors
            .iter()
            .map(|error| {
                error
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string())
            })
            .collect::<Vec<_>>()
            .join("; ");
        if !text.is_empty() {
            return Some(text);
        }
    }
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(collapsed.chars().take(180).collect())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::load_from;

    fn test_client(base: &str, read_only: bool) -> Client {
        let mut pairs = vec![
            ("REDMINE_URL".to_string(), base.to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
        ];
        if !read_only {
            pairs.push(("REDMINE_READONLY".to_string(), "false".to_string()));
        }
        let env: HashMap<String, String> = pairs.into_iter().collect();
        let config = load_from(&env, |_| unreachable!()).unwrap();
        Client::new(config).unwrap()
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn keeps_a_base_path_prefix() {
        let base = base_url("https://redmine.example/redmine").unwrap();
        let joined = build_url(&base, "/issues/1.json", &[("include", "journals")]).unwrap();
        assert_eq!(
            joined.as_str(),
            "https://redmine.example/redmine/issues/1.json?include=journals"
        );
    }

    #[test]
    fn absolute_urls_must_be_on_this_instance() {
        let base = base_url("https://redmine.example/redmine").unwrap();
        let ok = "https://redmine.example/redmine/attachments/download/5/a.txt";
        assert_eq!(build_url(&base, ok, &[]).unwrap().as_str(), ok);
        for refused in [
            "https://files.example/attachments/download/5/a.txt",
            "https://redmine.example:8443/redmine/attachments/5",
            "http://redmine.example/redmine/attachments/5",
            "https://redmine.example/other-app/attachments/5",
        ] {
            let err = build_url(&base, refused, &[]).unwrap_err();
            assert!(err.contains("Refusing to send the API key"), "{refused}");
        }
    }

    #[test]
    fn redirects_may_upgrade_but_not_leave_the_host_or_downgrade() {
        let http = base_url("http://redmine.example").unwrap();
        assert!(same_instance(
            &http,
            &url("https://redmine.example/issues/1.json")
        ));
        assert!(!same_instance(
            &http,
            &url("https://redmine.example:8443/issues/1.json")
        ));
        let https = base_url("https://redmine.example").unwrap();
        assert!(!same_instance(
            &https,
            &url("http://redmine.example/issues/1.json")
        ));
        assert!(!same_instance(&https, &url("https://sso.example/login")));
    }

    #[test]
    fn retry_after_honours_seconds_and_caps_at_ten() {
        let header = HeaderValue::from_static("0.01");
        assert_eq!(retry_after_ms(Some(&header)), 10);
        let header = HeaderValue::from_static("11");
        assert_eq!(retry_after_ms(Some(&header)), 10_000);
        assert_eq!(retry_after_ms(None), 2_000);
        let header = HeaderValue::from_static("soon");
        assert_eq!(retry_after_ms(Some(&header)), 2_000);
    }

    #[tokio::test]
    async fn get_retries_429_twice_then_reads_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/1.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issue": {"id": 1}})))
            .expect(1)
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/1.json"))
            .and(header("x-redmine-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0.01"))
            .expect(2)
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), true);
        let body: serde_json::Value = client.get_json("/issues/1.json", &[]).await.unwrap();
        assert_eq!(body["issue"]["id"], 1);
    }

    #[tokio::test]
    async fn put_is_not_retried_and_read_only_never_sends_it() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/issues/1.json"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0.01"))
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), false);
        let err = client
            .put_json("/issues/1.json", &json!({"issue": {"notes": "hi"}}))
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(429));

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        let client = test_client(&server.uri(), true);
        let err = client
            .put_json("/issues/1.json", &json!({"issue": {"notes": "hi"}}))
            .await
            .unwrap_err();
        assert_eq!(err, RedmineError::ReadOnly);
        assert_eq!(err.status(), None);
        assert!(err.to_string().contains("REDMINE_READONLY=false"));
    }

    #[tokio::test]
    async fn unauthorized_quotes_the_errors_array_and_a_subpath_is_preserved() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redmine/issues/1.json"))
            .and(query_param("include", "journals"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"errors": ["Invalid key"]})),
            )
            .mount(&server)
            .await;

        let client = test_client(&format!("{}/redmine", server.uri()), true);
        let err = client
            .get_json::<serde_json::Value>("/issues/1.json", &[("include", "journals")])
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(401));
        assert!(err.to_string().contains("REDMINE_API_KEY"));
        assert!(err.to_string().contains("Invalid key"));
        assert!(err.path().unwrap().starts_with("/redmine/issues/1.json"));
    }

    #[tokio::test]
    async fn redirect_to_another_host_is_refused_before_the_key_leaves() {
        let sso = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(0)
            .mount(&sso)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/1.json"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/login", sso.uri()).as_str()),
            )
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), true);
        let err = client
            .get_json::<serde_json::Value>("/issues/1.json", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, RedmineError::Transport { .. }), "{err:?}");
        assert!(err.to_string().contains("not REDMINE_URL"), "{err}");
    }

    #[tokio::test]
    async fn redirect_within_the_instance_is_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/1.json"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/issues/2.json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/2.json"))
            .and(header("x-redmine-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issue": {"id": 2}})))
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), true);
        let body: serde_json::Value = client.get_json("/issues/1.json", &[]).await.unwrap();
        assert_eq!(body["issue"]["id"], 2);
    }

    #[tokio::test]
    async fn an_empty_or_misshapen_body_is_an_error_not_an_empty_result() {
        #[derive(Debug, serde::Deserialize)]
        struct Issue {
            #[allow(dead_code)]
            id: u64,
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/empty.json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issue.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "seven"})))
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), true);
        let empty = client
            .get_json::<serde_json::Value>("/empty.json", &[])
            .await
            .unwrap_err();
        assert!(matches!(empty, RedmineError::Json { .. }), "{empty:?}");

        let misshapen = client
            .get_json::<Issue>("/issue.json", &[])
            .await
            .unwrap_err();
        assert!(
            misshapen.to_string().contains("invalid type"),
            "{misshapen}"
        );
    }

    #[tokio::test]
    async fn byte_limited_get_stops_at_the_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("0123456789"))
            .mount(&server)
            .await;

        let client = test_client(&server.uri(), true);
        assert_eq!(
            client.get_bytes_limited("/a.txt", 10).await.unwrap(),
            b"0123456789"
        );
        let err = client.get_bytes_limited("/a.txt", 9).await.unwrap_err();
        assert!(
            matches!(err, RedmineError::TooLarge { limit: 9, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn html_error_bodies_are_dropped() {
        let message = format_http_error(500, "/issues/1.json", "<html>nope</html>");
        assert!(!message.contains("nope"));
        assert!(message.contains("HTTP 500"));
    }
}
