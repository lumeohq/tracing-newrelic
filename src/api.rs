use flate2::{write::GzEncoder, Compression};
use futures_util::join;
use reqwest::{
    header::{CONTENT_ENCODING, CONTENT_TYPE},
    Client, RequestBuilder,
};
use serde::Serialize;
use std::cmp::max;
use std::fmt::Debug;
use std::time::Duration;
use tracing::info;
use tokio::time::sleep;

use super::types::{NewrLogs, NewrSpans};

#[derive(Clone)]
/// Api Endpoint
pub enum ApiEndpoint {
    /// United States, Default
    US,
    /// European Union
    EU,
    /// Custom
    Custom(String),
}

impl Default for ApiEndpoint {
    fn default() -> Self {
        ApiEndpoint::US
    }
}

/// New relic Api
pub struct Api {
    /// Log Api Endpoint
    pub log_endpoint: ApiEndpoint,
    /// Trace Api Endpoint
    pub trace_endpoint: ApiEndpoint,
    /// Api Key
    pub key: String,
    /// Http Client
    pub client: Client,
    /// Batch request size
    pub batch_size: usize,

    logs_queue: Vec<NewrLogs>,
    spans_queue: Vec<NewrSpans>,
}

impl Api {
    pub(crate) async fn push(&mut self, logs: NewrLogs, traces: NewrSpans) {
        log::debug!(
            "pushing logs and traces, logs_queue_len={}, spans_queue_len={}",
            self.logs_queue.len(),
            self.spans_queue.len(),
        );

        self.logs_queue.push(logs);
        self.spans_queue.push(traces);

        if self.logs_queue.len() >= self.batch_size || self.spans_queue.len() >= self.batch_size {
            self.flush().await
        }
    }

    pub(crate) async fn flush(&mut self) {
        if self.logs_queue.is_empty() && self.spans_queue.is_empty() {
            return;
        }

        log::debug!(
            "flushing logs and traces, logs_queue_len={}, spans_queue_len={}",
            self.logs_queue.len(),
            self.spans_queue.len(),
        );

        let mut logs_service = Service::new(&self.logs_queue);
        let mut trace_service = Service::new(&self.spans_queue);

        loop {
            use ServiceStatus::*;

            match join!(logs_service.send(self), trace_service.send(self)) {
                (Timeount(d1), Timeount(d2)) => sleep(max(d1, d2)).await,

                (Timeount(d), _) | (_, Timeount(d)) => sleep(d).await,

                (Finished, Finished) => {
                    log::info!(
                        "flushed logs and traces, logs_queue_len={}, spans_queue_len={}",
                        self.logs_queue.len(),
                        self.spans_queue.len(),
                    );

                    self.logs_queue.clear();
                    self.spans_queue.clear();
                    return;
                }

                _ => {}
            }
        }
    }
}

impl Default for Api {
    fn default() -> Self {
        Api {
            log_endpoint: ApiEndpoint::default(),
            trace_endpoint: ApiEndpoint::default(),
            key: String::new(),
            client: Client::new(),
            batch_size: 10,
            logs_queue: Vec::with_capacity(10),
            spans_queue: Vec::with_capacity(10),
        }
    }
}

impl From<String> for Api {
    fn from(key: String) -> Self {
        Api {
            key,
            ..Default::default()
        }
    }
}

impl From<&str> for Api {
    fn from(key: &str) -> Self {
        Api {
            key: key.to_string(),
            ..Default::default()
        }
    }
}

impl From<(String, ApiEndpoint)> for Api {
    fn from(t: (String, ApiEndpoint)) -> Self {
        Api {
            key: t.0,
            log_endpoint: t.1.clone(),
            trace_endpoint: t.1,
            ..Default::default()
        }
    }
}

enum ServiceStatus {
    // Need to wait before next sending
    #[allow(unused)]
    Timeount(Duration),

    // Have remaining data to be sent
    Remaining,

    // Finished, either success or failed
    Finished,
}

struct Service<'a, T: Sendable> {
    data: &'a [T],
    // number of items to send each request,
    batch_len: usize,
    #[allow(unused)]
    retry_count: u32,
}

impl<'a, T: Sendable> Service<'a, T> {
    fn new(data: &'a [T]) -> Self {
        Service {
            batch_len: data.len(),
            data,
            retry_count: 0,
        }
    }

    async fn send(&mut self, _api: &Api) -> ServiceStatus {
        // nothing to send
        if self.data.is_empty() {
            info!("Nothing to send");
            return ServiceStatus::Finished;
        }

        let (left, right) = self.data.split_at(self.batch_len);
        info!(data = ?left, "Sending data");

        self.data = right;

        if self.data.is_empty() {
            ServiceStatus::Finished
        } else {
            ServiceStatus::Remaining
        }
    }
}

trait Sendable: Debug {
    fn build_request(data: &[Self], api: &Api) -> RequestBuilder
    where
        Self: Sized;
}

impl Sendable for NewrLogs {
    fn build_request(data: &[NewrLogs], api: &Api) -> RequestBuilder {
        let url = match &api.log_endpoint {
            ApiEndpoint::US => "https://log-api.newrelic.com/log/v1".into(),
            ApiEndpoint::EU => "https://log-api.eu.newrelic.com/log/v1".into(),
            ApiEndpoint::Custom(domain) => format!("{domain}/log/v1"),
        };
        // https://docs.newrelic.com/docs/logs/log-api/introduction-log-api/#json-headers
        api.client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_ENCODING, "gzip")
            .header("Api-Key", &api.key)
            .body(to_gz(&data))
    }
}

impl Sendable for NewrSpans {
    fn build_request(data: &[NewrSpans], api: &Api) -> RequestBuilder {
        let url = match &api.log_endpoint {
            ApiEndpoint::US => "https://trace-api.newrelic.com/trace/v1".into(),
            ApiEndpoint::EU => "https://trace-api.eu.newrelic.com/trace/v1".into(),
            ApiEndpoint::Custom(domain) => format!("{domain}/trace/v1"),
        };
        // https://docs.newrelic.com/docs/distributed-tracing/trace-api/trace-api-general-requirements-limits/#headers-query-parameters
        api.client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_ENCODING, "gzip")
            .header("Api-Key", &api.key)
            .header("Data-Format", "newrelic")
            .header("Data-Format-Version", "1")
            .body(to_gz(&data))
    }
}

#[inline]
fn to_gz<T: Serialize>(data: T) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    serde_json::to_writer(&mut encoder, &data).unwrap();
    encoder.finish().unwrap()
}
