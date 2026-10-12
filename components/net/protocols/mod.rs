/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::hash_map::Entry;
use std::future;
use std::future::Future;
use std::ops::Bound;
use std::pin::Pin;

use headers::Range;
use http::StatusCode;
use log::error;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::filemanager_thread::RelativePos;
use net_traits::request::{BodyChunkRequest, BodyChunkResponse, Request};
use net_traits::response::Response;
use net_traits::{DiscardFetch, NetworkError};
use rustc_hash::FxHashMap;
use servo_url::ServoUrl;
use url::Position;

use crate::fetch::methods::{DoneChannel, FetchContext, RangeRequestBounds, fetch};

mod blob;
mod data;
mod file;

use blob::BlobProtocolHander;
use data::DataProtocolHander;
use file::FileProtocolHander;

type FutureResponse<'a> = Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

// The set of schemes that can't be registered.
static FORBIDDEN_SCHEMES: [&str; 4] = ["http", "https", "chrome", "about"];

pub trait ProtocolHandler: Send + Sync {
    /// A list of schema-less URLs that can be resolved against this handler's
    /// scheme. These URLs will be granted access to the `navigator.servo`
    /// interface to perform privileged operations that manipulate Servo internals.
    fn privileged_paths(&self) -> &'static [&'static str] {
        &[]
    }

    /// Triggers the load of a resource for this protocol and returns a future
    /// that will produce a Response. Even if the protocol is not backed by a
    /// http endpoint, it is recommended to a least provide:
    /// - A relevant status code.
    /// - A Content Type.
    fn load<'a>(
        &'a self,
        request: &'a mut Request,
        done_chan: &mut DoneChannel,
        context: &FetchContext,
    ) -> FutureResponse<'a>;

    /// Specify if resources served by that protocol can be retrieved
    /// with `fetch()` without no-cors mode to allow the caller direct
    /// access to the resource content.
    fn is_fetchable(&self) -> bool {
        false
    }

    /// Specify if this custom protocol can be used in a [secure context]
    ///
    /// Note: this only works for bypassing mixed content checks right now
    ///
    /// [secure context]: https://developer.mozilla.org/en-US/docs/Web/Security/Secure_Contexts
    fn is_secure(&self) -> bool {
        false
    }

    /// Specify if this custom protocol is a standard scheme, as `http`: its URLs get a tuple
    /// origin (scheme, host, port) rather than an opaque one, so an app served under it is
    /// same-origin with itself (module scripts, `fetch`, storage and CSP `'self'` all work). A
    /// standard scheme must also be [secure](Self::is_secure); its origin is then potentially
    /// trustworthy. For an embedder's own app scheme, as Chromium's privileged schemes.
    fn is_standard(&self) -> bool {
        false
    }
}

/// The whole body of `request` (empty when it has none), for a protocol handler that accepts
/// uploads such as a `POST` from `fetch`. Fails when the body stream fails or exceeds `limit`
/// bytes.
pub fn read_request_body(
    request: &Request,
    limit: usize,
) -> impl Future<Output = Result<Vec<u8>, NetworkError>> + Send + 'static {
    let stream = request.body.as_ref().map(|body| body.clone_stream());
    async move {
        let Some(stream) = stream else {
            return Ok(Vec::new());
        };
        let failed = |why: &str| NetworkError::ResourceLoadError(format!("Request body: {why}"));
        tokio::task::spawn_blocking(move || {
            let (sender, receiver) =
                ipc_channel::ipc::channel::<BodyChunkResponse>().map_err(|_| failed("no channel"))?;
            let request_chunk = |message: BodyChunkRequest| {
                stream
                    .lock()
                    .as_ref()
                    .ok_or_else(|| failed("the stream is closed"))?
                    .send(message)
                    .map_err(|_| failed("the stream is closed"))
            };
            request_chunk(BodyChunkRequest::Connect(sender))?;
            request_chunk(BodyChunkRequest::Chunk)?;
            let mut body = Vec::new();
            loop {
                match receiver.recv().map_err(|_| failed("the stream ended"))? {
                    BodyChunkResponse::Chunk(bytes) => {
                        if body.len() + bytes.len() > limit {
                            return Err(failed("too large"));
                        }
                        body.extend_from_slice(&bytes);
                        request_chunk(BodyChunkRequest::Chunk)?;
                    },
                    BodyChunkResponse::Done => return Ok(body),
                    BodyChunkResponse::Error => return Err(failed("the stream failed")),
                }
            }
        })
        .await
        .map_err(|_| failed("the reader stopped"))?
    }
}

#[derive(Default)]
pub struct ProtocolRegistry {
    pub(crate) handlers: FxHashMap<String, Box<dyn ProtocolHandler>>, // Maps scheme -> handler
}

#[derive(Clone, Copy, Debug)]
pub enum ProtocolRegisterError {
    ForbiddenScheme,
    SchemeAlreadyRegistered,
}

impl ProtocolRegistry {
    pub fn with_internal_protocols() -> Self {
        let mut registry = Self::default();
        // We just created a new registry, and know that we aren't using
        // any forbidden schemes, so this should never panic.
        registry
            .register("data", DataProtocolHander::default())
            .expect("Infallible");
        registry
            .register("blob", BlobProtocolHander::default())
            .expect("Infallible");
        registry
            .register("file", FileProtocolHander::default())
            .expect("Infallible");
        registry
    }

    /// Do not allow users to enter an arbitrary protocol as this can lead to slowdowns.
    pub fn register(
        &mut self,
        scheme: &str,
        handler: impl ProtocolHandler + 'static,
    ) -> Result<(), ProtocolRegisterError> {
        if FORBIDDEN_SCHEMES.contains(&scheme) {
            error!("Protocol handler for '{scheme}' is not allowed to be registered.");
            return Err(ProtocolRegisterError::ForbiddenScheme);
        }

        if let Entry::Vacant(entry) = self.handlers.entry(scheme.into()) {
            if handler.is_standard() && handler.is_secure() {
                servo_url::register_standard_secure_scheme(scheme);
            }
            entry.insert(Box::new(handler));
            Ok(())
        } else {
            error!("Protocol handler for '{scheme}' is already registered.");
            Err(ProtocolRegisterError::SchemeAlreadyRegistered)
        }
    }

    pub fn register_page_content_handler(
        &mut self,
        scheme: String,
        url: String,
    ) -> Result<(), ProtocolRegisterError> {
        self.register(
            &scheme.clone(),
            WebPageContentProtocolHandler { url, scheme },
        )
    }

    pub fn get(&self, scheme: &str) -> Option<&dyn ProtocolHandler> {
        self.handlers.get(scheme).map(|e| e.as_ref())
    }

    pub fn merge(&mut self, mut other: ProtocolRegistry) {
        for (scheme, handler) in other.handlers.drain() {
            if FORBIDDEN_SCHEMES.contains(&scheme.as_str()) {
                error!("Protocol handler for '{scheme}' is not allowed to be registered.");
                continue;
            }

            self.handlers.entry(scheme).or_insert(handler);
        }
    }

    pub fn is_fetchable(&self, scheme: &str) -> bool {
        self.handlers
            .get(scheme)
            .is_some_and(|handler| handler.is_fetchable())
    }

    pub fn is_secure(&self, scheme: &str) -> bool {
        self.handlers
            .get(scheme)
            .is_some_and(|handler| handler.is_secure())
    }

    pub fn privileged_urls(&self) -> Vec<ServoUrl> {
        self.handlers
            .iter()
            .flat_map(|(scheme, handler)| {
                let paths = handler.privileged_paths();
                paths
                    .iter()
                    .filter_map(move |path| ServoUrl::parse(&format!("{scheme}:{path}")).ok())
            })
            .collect()
    }
}

struct WebPageContentProtocolHandler {
    url: String,
    scheme: String,
}

impl ProtocolHandler for WebPageContentProtocolHandler {
    /// <https://html.spec.whatwg.org/multipage/#protocol-handler-invocation>
    fn load<'a>(
        &'a self,
        request: &'a mut Request,
        _done_chan: &mut DoneChannel,
        context: &FetchContext,
    ) -> FutureResponse<'a> {
        let mut url = request.current_url();
        // Step 1. Assert: inputURL's scheme is normalizedScheme.
        assert!(url.scheme() == self.scheme);
        // Step 2. Set the username given inputURL and the empty string.
        //
        // Ignore errors if no username can be set, which depending on the
        // URL in the scheme handler might be bogus. For example, with a
        // `mailto:` handler, it doesn't have a base. See the
        // documentation at [`url::Url::set_username`]
        let _ = url.set_username("");

        // Step 3. Set the password given inputURL and the empty string.
        //
        // Ignore errors if no password can be set, which depending on the
        // URL in the scheme handler might be bogus. For example, with a
        // `mailto:` handler, it doesn't have a base. See the
        // documentation at [`url::Url::set_password`]
        let _ = url.set_password(None);

        // Step 4. Let inputURLString be the serialization of inputURL.
        // Step 5. Let encodedURL be the result of running UTF-8 percent-encode on inputURLString using the component percent-encode set.
        //
        // Url is already UTF-8, so encoding isn't required
        let encoded_url = &url[Position::AfterScheme..][1..];
        // Step 6. Let handlerURLString be normalizedURLString.
        // Step 7. Replace the first instance of "%s" in handlerURLString with encodedURL.
        let handler_url_string = self.url.replacen("%s", encoded_url, 1);
        // Step 8. Let resultURL be the result of parsing handlerURLString.
        let Ok(result_url) = ServoUrl::parse(&handler_url_string) else {
            return Box::pin(future::ready(Response::network_error(
                NetworkError::ProtocolHandlerSubstitutionError,
            )));
        };
        // Ensure we did a proper substitution with a HTTP result
        assert!(matches!(result_url.scheme(), "http" | "https"));
        // Step 9. Navigate an appropriate navigable to resultURL.
        request
            .url_list
            .push(UrlWithBlobClaim::new(result_url, None));
        let request2 = request.clone();
        let context2 = context.clone();
        Box::pin(async move { fetch(request2, &mut DiscardFetch, &context2).await })
    }
}

/// Test if the URL is potentially trustworthy or the custom protocol is registered as secure
pub fn is_url_potentially_trustworthy(
    protocol_registry: &ProtocolRegistry,
    url: &ServoUrl,
) -> bool {
    url.is_potentially_trustworthy() || protocol_registry.is_secure(url.scheme())
}

pub fn range_not_satisfiable_error(response: &mut Response) {
    response.status = StatusCode::RANGE_NOT_SATISFIABLE.into();
}

/// Get the range bounds if the `Range` header is present.
pub fn get_range_request_bounds(range: Option<Range>, len: u64) -> RangeRequestBounds {
    if let Some(ref range) = range {
        let (start, end) = match range.satisfiable_ranges(len).next() {
            Some((Bound::Included(start), Bound::Unbounded)) => (start, None),
            Some((Bound::Included(start), Bound::Included(end))) => {
                // `end` should be less or equal to `start`.
                (start, Some(i64::max(start as i64, end as i64)))
            },
            Some((Bound::Unbounded, Bound::Included(offset))) => {
                return RangeRequestBounds::Pending(offset);
            },
            _ => (0, None),
        };
        RangeRequestBounds::Final(RelativePos::from_opts(Some(start as i64), end))
    } else {
        RangeRequestBounds::Final(RelativePos::from_opts(Some(0), None))
    }
}

pub fn partial_content(response: &mut Response) {
    response.status = StatusCode::PARTIAL_CONTENT.into();
}
