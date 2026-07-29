//! Configurable transport hooks for WASM targets.
//!
//! Enable the `wasm-bindgen` feature to use the browser Fetch API. Runtimes that
//! do not use `wasm-bindgen` must install their own handlers before sending the
//! first request.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use std::sync::OnceLock;

use crate::request::ParsedRequest;
use crate::{Error, Method};

/// The future returned by a WASM request handler.
pub type SendFuture = Pin<Box<dyn Future<Output = Result<Response, Error>>>>;

/// Function used to send a request in a WASM runtime.
///
/// The handler must enforce [`Request::timeout`]. It should also enforce the
/// response size limits while reading so oversized responses are not fully
/// buffered; bitreq validates both limits again after the handler returns.
pub type SendHandler = fn(Request) -> SendFuture;

/// Runtime-specific handlers used to send WASM requests.
#[derive(Clone, Copy, Debug)]
pub struct Handlers {
    send: SendHandler,
}

impl Handlers {
    /// Creates a handler set from a request function.
    pub const fn new(send: SendHandler) -> Self { Self { send } }
}

/// A request passed to the configured WASM transport.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Request {
    /// HTTP method.
    pub method: Method,
    /// Fully parsed URL, including encoded query parameters.
    pub url: String,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Optional request timeout, which the handler must enforce.
    pub timeout: Option<Duration>,
    /// Maximum total response-header size.
    ///
    /// Handlers should enforce this while receiving headers.
    pub max_headers_size: Option<usize>,
    /// Maximum response-body size.
    ///
    /// Handlers should enforce this while receiving the body.
    pub max_body_size: Option<usize>,
}

/// A response returned by a configured WASM transport.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Response {
    /// HTTP status code.
    pub status_code: i32,
    /// HTTP reason phrase.
    pub reason_phrase: String,
    /// Response headers.
    ///
    /// Bitreq normalizes field names to lowercase before exposing the response.
    pub headers: BTreeMap<String, String>,
    /// Final response URL.
    pub url: String,
    /// Response body.
    ///
    /// Bitreq discards this for `HEAD`, 204, and 304 responses.
    pub body: Vec<u8>,
}

impl Response {
    /// Creates a response from transport-provided parts.
    pub fn new(
        status_code: i32,
        reason_phrase: String,
        headers: BTreeMap<String, String>,
        url: String,
        body: Vec<u8>,
    ) -> Self {
        Self { status_code, reason_phrase, headers, url, body }
    }
}

static HANDLERS: OnceLock<Handlers> = OnceLock::new();

/// Installs the handlers used for WASM requests.
///
/// Call this before sending the first request. The handlers can only be set once.
///
/// # Errors
///
/// Returns the supplied handlers if handlers were already installed or the
/// built-in browser handlers were already used.
pub fn set_handlers(handlers: Handlers) -> Result<(), Handlers> { HANDLERS.set(handlers) }

fn handlers() -> Result<&'static Handlers, Error> {
    if let Some(handlers) = HANDLERS.get() {
        return Ok(handlers);
    }

    #[cfg(feature = "wasm-bindgen")]
    {
        Ok(HANDLERS.get_or_init(crate::wasm_bindgen::handlers))
    }
    #[cfg(not(feature = "wasm-bindgen"))]
    {
        Err(Error::Wasm(String::from(
            "no WASM handlers installed; call bitreq::wasm::set_handlers",
        )))
    }
}

pub(crate) async fn send(request: ParsedRequest) -> Result<crate::Response, Error> {
    let method = request.config.method.clone();
    let max_headers_size = request.config.max_headers_size;
    let max_body_size = request.config.max_body_size;
    let request = Request {
        method: request.config.method,
        url: String::from(request.url.as_str()),
        headers: request.config.headers,
        body: request.config.body,
        timeout: request.timeout,
        max_headers_size,
        max_body_size,
    };
    let response = (handlers()?.send)(request).await?;

    finalize_response(method, max_headers_size, max_body_size, response)
}

fn finalize_response(
    method: Method,
    max_headers_size: Option<usize>,
    max_body_size: Option<usize>,
    mut response: Response,
) -> Result<crate::Response, Error> {
    if let Some(max) = max_headers_size {
        let headers_size = response.headers.iter().try_fold(0usize, |total, (name, value)| {
            total.checked_add(name.len())?.checked_add(value.len())?.checked_add(4)
        });
        if match headers_size {
            Some(size) => size > max,
            None => true,
        } {
            return Err(Error::HeadersOverflow);
        }
    }

    let mut headers = BTreeMap::new();
    for (mut name, value) in response.headers {
        name.make_ascii_lowercase();
        headers.insert(name, value);
    }
    response.headers = headers;

    if method == Method::Head || response.status_code == 204 || response.status_code == 304 {
        response.body.clear();
    }
    if max_body_size.is_some_and(|max| response.body.len() > max) {
        return Err(Error::BodyOverflow);
    }

    Ok(crate::Response::from_parts(
        response.status_code,
        response.reason_phrase,
        response.headers,
        response.url,
        response.body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Response {
        Response::new(
            200,
            String::from("OK"),
            BTreeMap::from([(String::from("Content-Type"), String::from("text/plain"))]),
            String::from("https://example.com/"),
            vec![1, 2, 3],
        )
    }

    #[test]
    fn normalizes_response_headers() {
        let response = finalize_response(Method::Get, None, None, response()).unwrap();

        assert_eq!(response.headers.get("content-type"), Some(&String::from("text/plain")));
        assert!(!response.headers.contains_key("Content-Type"));
    }

    #[test]
    fn counts_headers_before_normalizing_names() {
        let mut input = response();
        input.headers = BTreeMap::from([
            (String::from("X-Test"), String::from("a")),
            (String::from("x-test"), String::from("b")),
        ]);

        assert!(matches!(
            finalize_response(Method::Get, Some(11), None, input),
            Err(Error::HeadersOverflow)
        ));
    }

    #[test]
    fn discards_body_when_http_semantics_require_it() {
        for (method, status_code) in [(Method::Head, 200), (Method::Get, 204), (Method::Get, 304)] {
            let mut input = response();
            input.status_code = status_code;

            let response = finalize_response(method, None, Some(0), input).unwrap();
            assert!(response.as_bytes().is_empty());
        }
    }

    #[test]
    fn rejects_oversized_response() {
        assert!(matches!(
            finalize_response(Method::Get, Some(0), None, response()),
            Err(Error::HeadersOverflow)
        ));
        assert!(matches!(
            finalize_response(Method::Get, None, Some(2), response()),
            Err(Error::BodyOverflow)
        ));
    }
}
