use std::future::Future;
use std::time::SystemTime;

use futures::FutureExt;
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use reqwest::{Request, Response};
use reqwest_middleware::ClientWithMiddleware;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{debug, info_span, instrument, trace, warn, Instrument};

use puffin_cache::{CacheEntry, Freshness};
use puffin_fs::write_atomic;

use crate::cache_headers::CacheHeaders;

/// Either a cached client error or a (user specified) error from the callback
#[derive(Debug)]
pub enum CachedClientError<CallbackError> {
    Client(crate::Error),
    Callback(CallbackError),
}

impl<CallbackError> From<crate::Error> for CachedClientError<CallbackError> {
    fn from(error: crate::Error) -> Self {
        CachedClientError::Client(error)
    }
}

impl From<CachedClientError<crate::Error>> for crate::Error {
    fn from(error: CachedClientError<crate::Error>) -> crate::Error {
        match error {
            CachedClientError::Client(error) => error,
            CachedClientError::Callback(error) => error,
        }
    }
}

#[derive(Debug)]
enum CachedResponse<Payload: Serialize> {
    /// The cached response is fresh without an HTTP request (e.g. immutable)
    FreshCache(Payload),
    /// The cached response is fresh after an HTTP request (e.g. 304 not modified)
    NotModified(DataWithCachePolicy<Payload>),
    /// There was no prior cached response or the cache was outdated
    ///
    /// The cache policy is `None` if it isn't storable
    ModifiedOrNew(Response, Option<Box<CachePolicy>>),
}

/// Serialize the actual payload together with its caching information.
#[derive(Debug, Deserialize, Serialize)]
pub struct DataWithCachePolicy<Payload: Serialize> {
    pub data: Payload,
    /// Whether the response should be considered immutable.
    immutable: bool,
    /// The [`CachePolicy`] is used to determine if the response is fresh or stale.
    /// The policy is large (448 bytes at time of writing), so we reduce the stack size by
    /// boxing it.
    cache_policy: Box<CachePolicy>,
}

/// Custom caching layer over [`reqwest::Client`] using `http-cache-semantics`.
///
/// The implementation takes inspiration from the `http-cache` crate, but adds support for running
/// an async callback on the response before caching. We use this to e.g. store a
/// parsed version of the wheel metadata and for our remote zip reader. In the latter case, we want
/// to read a single file from a remote zip using range requests (so we don't have to download the
/// entire file). We send a HEAD request in the caching layer to check if the remote file has
/// changed (and if range requests are supported), and in the callback we make the actual range
/// requests if required.
///
/// Unlike `http-cache`, all outputs must be serde-able. Currently everything is json, but we can
/// transparently switch to a faster/smaller format.
///
/// Again unlike `http-cache`, the caller gets full control over the cache key with the assumption
/// that it's a file.
#[derive(Debug, Clone)]
pub struct CachedClient(ClientWithMiddleware);

impl CachedClient {
    pub fn new(client: ClientWithMiddleware) -> Self {
        Self(client)
    }

    /// The middleware is the retry strategy
    pub fn uncached(&self) -> ClientWithMiddleware {
        self.0.clone()
    }

    /// Make a cached request with a custom response transformation
    ///
    /// If a new response was received (no prior cached response or modified on the remote), the
    /// response is passed through `response_callback` and only the result is cached and returned.
    /// The `response_callback` is allowed to make subsequent requests, e.g. through the uncached
    /// client.
    #[instrument(skip_all)]
    pub async fn get_cached_with_callback<
        Payload: Serialize + DeserializeOwned + Send,
        CallBackError,
        Callback,
        CallbackReturn,
    >(
        &self,
        req: Request,
        cache_entry: &CacheEntry,
        cache_control: CacheControl,
        response_callback: Callback,
    ) -> Result<Payload, CachedClientError<CallBackError>>
    where
        Callback: FnOnce(Response) -> CallbackReturn,
        CallbackReturn: Future<Output = Result<Payload, CallBackError>>,
    {
        let read_span = info_span!("read_cache", file = %cache_entry.path().display());
        let read_result = fs_err::tokio::read(cache_entry.path())
            .instrument(read_span)
            .await;
        let cached = if let Ok(cached) = read_result {
            let parse_span = info_span!(
                "parse_cache",
                path = %cache_entry.path().display()
            );
            let parse_result = parse_span
                .in_scope(|| rmp_serde::from_slice::<DataWithCachePolicy<Payload>>(&cached));
            match parse_result {
                Ok(data) => Some(data),
                Err(err) => {
                    warn!(
                        "Broken cache entry at {}, removing: {err}",
                        cache_entry.path().display()
                    );
                    let _ = fs_err::tokio::remove_file(&cache_entry.path()).await;
                    None
                }
            }
        } else {
            None
        };

        let cached_response = self.send_cached(req, cache_control, cached).boxed().await?;

        let write_cache = info_span!("write_cache", file = %cache_entry.path().display());
        match cached_response {
            CachedResponse::FreshCache(data) => Ok(data),
            CachedResponse::NotModified(data_with_cache_policy) => {
                async {
                    let data =
                        rmp_serde::to_vec(&data_with_cache_policy).map_err(crate::Error::from)?;
                    write_atomic(cache_entry.path(), data)
                        .await
                        .map_err(crate::Error::CacheWrite)?;
                    Ok(data_with_cache_policy.data)
                }
                .instrument(write_cache)
                .await
            }
            CachedResponse::ModifiedOrNew(res, cache_policy) => {
                let headers = CacheHeaders::from_response(res.headers().get_all("cache-control"));
                let immutable = headers.is_immutable();

                let data = response_callback(res)
                    .await
                    .map_err(|err| CachedClientError::Callback(err))?;
                if let Some(cache_policy) = cache_policy {
                    let data_with_cache_policy = DataWithCachePolicy {
                        data,
                        immutable,
                        cache_policy,
                    };
                    async {
                        fs_err::tokio::create_dir_all(cache_entry.dir())
                            .await
                            .map_err(crate::Error::CacheWrite)?;
                        let data = rmp_serde::to_vec(&data_with_cache_policy)
                            .map_err(crate::Error::from)?;
                        write_atomic(cache_entry.path(), data)
                            .await
                            .map_err(crate::Error::CacheWrite)?;
                        Ok(data_with_cache_policy.data)
                    }
                    .instrument(write_cache)
                    .await
                } else {
                    Ok(data)
                }
            }
        }
    }

    /// `http-cache-semantics` to `reqwest` wrapper
    async fn send_cached<T: Serialize + DeserializeOwned>(
        &self,
        mut req: Request,
        cache_control: CacheControl,
        cached: Option<DataWithCachePolicy<T>>,
    ) -> Result<CachedResponse<T>, crate::Error> {
        // The converted types are from the specific `reqwest` types to the more generic `http`
        // types.
        let mut converted_req = http::Request::try_from(
            req.try_clone()
                .expect("You can't use streaming request bodies with this function"),
        )?;

        let url = req.url().clone();
        let cached_response = if let Some(cached) = cached {
            // Avoid sending revalidation requests for immutable responses.
            if cached.immutable && !cached.cache_policy.is_stale(SystemTime::now()) {
                debug!("Found immutable response for: {url}");
                return Ok(CachedResponse::FreshCache(cached.data));
            }

            // Apply the cache control header, if necessary.
            match cache_control {
                CacheControl::None => {}
                CacheControl::MustRevalidate => {
                    converted_req.headers_mut().insert(
                        http::header::CACHE_CONTROL,
                        http::HeaderValue::from_static("max-age=0, must-revalidate"),
                    );
                }
            }

            match cached
                .cache_policy
                .before_request(&converted_req, SystemTime::now())
            {
                BeforeRequest::Fresh(_) => {
                    debug!("Found fresh response for: {url}");
                    CachedResponse::FreshCache(cached.data)
                }
                BeforeRequest::Stale { request, matches } => {
                    if !matches {
                        // This shouldn't happen; if it does, we'll override the cache.
                        warn!("Cached request doesn't match current request for: {url}");
                        return self.fresh_request(req, converted_req).await;
                    }

                    debug!("Sending revalidation request for: {url}");
                    for header in &request.headers {
                        req.headers_mut().insert(header.0.clone(), header.1.clone());
                        converted_req
                            .headers_mut()
                            .insert(header.0.clone(), header.1.clone());
                    }
                    let res = self
                        .0
                        .execute(req)
                        .instrument(info_span!("revalidation_request", url = url.as_str()))
                        .await?
                        .error_for_status()?;
                    let mut converted_res = http::Response::new(());
                    *converted_res.status_mut() = res.status();
                    for header in res.headers() {
                        converted_res.headers_mut().insert(
                            http::HeaderName::from(header.0),
                            http::HeaderValue::from(header.1),
                        );
                    }
                    let after_response = cached.cache_policy.after_response(
                        &converted_req,
                        &converted_res,
                        SystemTime::now(),
                    );
                    match after_response {
                        AfterResponse::NotModified(new_policy, _parts) => {
                            debug!("Found not-modified response for: {url}");
                            let headers =
                                CacheHeaders::from_response(res.headers().get_all("cache-control"));
                            let immutable = headers.is_immutable();
                            CachedResponse::NotModified(DataWithCachePolicy {
                                data: cached.data,
                                immutable,
                                cache_policy: Box::new(new_policy),
                            })
                        }
                        AfterResponse::Modified(new_policy, _parts) => {
                            debug!("Found modified response for: {url}");
                            CachedResponse::ModifiedOrNew(
                                res,
                                new_policy.is_storable().then(|| Box::new(new_policy)),
                            )
                        }
                    }
                }
            }
        } else {
            debug!("No cache entry for: {url}");
            self.fresh_request(req, converted_req).await?
        };
        Ok(cached_response)
    }

    #[instrument(skip_all, fields(url = req.url().as_str()))]
    async fn fresh_request<T: Serialize>(
        &self,
        req: Request,
        converted_req: http::Request<reqwest::Body>,
    ) -> Result<CachedResponse<T>, crate::Error> {
        trace!("{} {}", req.method(), req.url());
        let res = self.0.execute(req).await?.error_for_status()?;
        let mut converted_res = http::Response::new(());
        *converted_res.status_mut() = res.status();
        for header in res.headers() {
            converted_res.headers_mut().insert(
                http::HeaderName::from(header.0),
                http::HeaderValue::from(header.1),
            );
        }
        let cache_policy =
            CachePolicy::new(&converted_req.into_parts().0, &converted_res.into_parts().0);
        Ok(CachedResponse::ModifiedOrNew(
            res,
            cache_policy.is_storable().then(|| Box::new(cache_policy)),
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CacheControl {
    /// Respect the `cache-control` header from the response.
    None,
    /// Apply `max-age=0, must-revalidate` to the request.
    MustRevalidate,
}

impl From<Freshness> for CacheControl {
    fn from(value: Freshness) -> Self {
        match value {
            Freshness::Fresh => CacheControl::None,
            Freshness::Stale => CacheControl::MustRevalidate,
            Freshness::Missing => CacheControl::None,
        }
    }
}
