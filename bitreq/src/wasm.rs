use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use core::cell::Cell;
use core::time::Duration;
use std::io;

use js_sys::{Function, Promise, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::{wasm_bindgen, UnwrapThrowExt};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{AbortController, AbortSignal, Headers, RequestInit};

use crate::request::ParsedRequest;
use crate::{Error, Response};

const FETCH_TIMEOUT_REASON: &str = "reqwest::errors::TimedOut";

fn wasm_error(value: JsValue) -> Error { Error::Wasm(format!("{value:?}")) }

fn timeout_error() -> Error {
    Error::IoError(io::Error::new(
        io::ErrorKind::TimedOut,
        "the timeout of the request was reached",
    ))
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = "setTimeout")]
    fn set_timeout(handler: &Function, timeout: i32) -> JsValue;

    #[wasm_bindgen(js_name = "clearTimeout")]
    fn clear_timeout(handle: JsValue) -> JsValue;
}

async fn promise<T, F>(promise: js_sys::Promise, map_rejection: F) -> Result<T, Error>
where
    T: JsCast,
    F: FnOnce(JsValue) -> Error,
{
    use wasm_bindgen_futures::JsFuture;

    let js_val = JsFuture::from(promise).await.map_err(map_rejection)?;

    js_val
        .dyn_into::<T>()
        .map_err(|_js_val| Error::Wasm(String::from("promise resolved to unexpected type")))
}

/// A guard that cancels a fetch request when dropped.
struct AbortGuard {
    ctrl: AbortController,
    // Set only by our timeout callback. Browsers do not always reject timed-out
    // fetches with the abort reason below: once headers have arrived, a timeout
    // during `Response.arrayBuffer()` can surface as a generic `AbortError`.
    // Keeping this Rust-side flag lets us report `with_timeout` failures
    // consistently without parsing browser-specific JavaScript error strings.
    //
    // This is separate from AbortSignal::aborted so only our timeout callback
    // can classify an error as a timeout. Some browsers also lose the custom
    // abort reason when the abort happens while reading the response body.
    timed_out: Rc<Cell<bool>>,
    timeout: Option<(JsValue, Closure<dyn FnMut()>)>,
}

impl AbortGuard {
    fn new() -> Result<Self, Error> {
        Ok(AbortGuard {
            ctrl: AbortController::new().map_err(wasm_error)?,
            timed_out: Rc::new(Cell::new(false)),
            timeout: None,
        })
    }

    fn signal(&self) -> AbortSignal { self.ctrl.signal() }

    fn timeout(&mut self, timeout: Duration) {
        let ctrl = self.ctrl.clone();
        let timed_out = Rc::clone(&self.timed_out);
        let abort = Closure::once(move || {
            // Mark the request before aborting it so the rejection handler can
            // classify either a `fetch()` rejection or a later body-read
            // rejection as a timeout.
            timed_out.set(true);
            ctrl.abort_with_reason(&FETCH_TIMEOUT_REASON.into());
        });
        let timeout = set_timeout(
            abort.as_ref().unchecked_ref::<js_sys::Function>(),
            timeout.as_millis().try_into().expect("timeout"),
        );
        if let Some((id, _)) = self.timeout.replace((timeout, abort)) {
            clear_timeout(id);
        }
    }

    fn map_rejection(&self, value: JsValue) -> Error {
        // The same abort signal is attached to the request and remains active
        // while we read the response body. If the signal was tripped by our
        // timer, the user-visible error should be bitreq's timeout error no
        // matter which browser-level promise rejected.
        if self.timed_out.get() {
            timeout_error()
        } else {
            wasm_error(value)
        }
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.ctrl.abort();
        if let Some((id, _)) = self.timeout.take() {
            clear_timeout(id);
        }
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = fetch)]
    fn fetch_with_request(input: &web_sys::Request) -> Promise;
}

fn js_fetch(req: &web_sys::Request) -> Promise {
    use wasm_bindgen::JsCast;
    let global = js_sys::global();

    if let Some(scope) = global.dyn_ref::<web_sys::ServiceWorkerGlobalScope>() {
        scope.fetch_with_request(req)
    } else {
        fetch_with_request(req)
    }
}

pub(crate) async fn send(request: ParsedRequest) -> Result<Response, Error> {
    let init = RequestInit::new();
    init.set_method(&request.config.method.to_string());

    let headers = Headers::new().map_err(wasm_error)?;
    for (key, value) in &request.config.headers {
        headers.append(key, value).map_err(wasm_error)?;
    }
    init.set_headers(&headers);

    if let Some(body) = request.config.body {
        if !body.is_empty() {
            init.set_body(&JsValue::from(body));
        }
    }

    let mut abort = AbortGuard::new()?;
    if let Some(timeout) = request.timeout {
        abort.timeout(timeout);
    }
    init.set_signal(Some(&abort.signal()));

    let js_req =
        web_sys::Request::new_with_str_and_init(request.url.as_str(), &init).map_err(wasm_error)?;
    let p = js_fetch(&js_req);
    // Covers timeouts before the browser resolves the `fetch()` promise, i.e.
    // before response headers are available.
    let response = promise::<web_sys::Response, _>(p, |value| abort.map_rejection(value)).await?;

    let status_code = i32::from(response.status());
    let reason_phrase = response.status_text();
    let url = response.url();
    let js_headers = response.headers();
    let mut remaining_headers_size = request.config.max_headers_size;
    let mut headers = BTreeMap::new();
    for item in js_headers.entries() {
        let item = item.expect_throw("headers iterator doesn't throw");
        let item: js_sys::Array = item.dyn_into().expect_throw("header item is an array");

        let name = item.get(0).as_string().expect_throw("header name is a string");

        let value = item.get(1).as_string().expect_throw("header value is a string");

        if let Some(remaining) = remaining_headers_size.as_mut() {
            let header_size = name.len().saturating_add(value.len()).saturating_add(4);
            if header_size > *remaining {
                return Err(Error::HeadersOverflow);
            }
            *remaining -= header_size;
        }

        headers.insert(name, value);
    }

    // Covers timeouts after headers have arrived but before the full response
    // body has been buffered. Chromium reports this as a generic AbortError, so
    // it must use the same guard state instead of matching on the JS error text.
    let body = promise::<wasm_bindgen::JsValue, _>(
        response.array_buffer().map_err(|value| abort.map_rejection(value))?,
        |value| abort.map_rejection(value),
    )
    .await?;

    let buffer = Uint8Array::new(&body);
    let len = buffer.length() as usize;
    if request.config.max_body_size.is_some_and(|max| len > max) {
        return Err(Error::BodyOverflow);
    }
    let bytes = buffer.to_vec();

    Ok(Response::from_parts(status_code, reason_phrase, headers, url, bytes))
}
