//! An in-memory [`CacheProvider`] backed by a single `ConstLru`

use const_lru::ConstLru;
use http::{
    header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH, LAST_MODIFIED},
    HeaderMap, HeaderValue,
};
use http_body::Body;
use http_body_util::BodyExt;
use num_traits::{PrimInt, Unsigned};
use std::{alloc::alloc, alloc::Layout, error::Error, ptr::addr_of_mut, time::SystemTime};
use time::{format_description::well_known::Rfc2822, OffsetDateTime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::PollSender;

use crate::{
    base64_blake3_body_etag::base64_blake3_body_etag,
    simple_etag_cache_key::{calc_simple_etag_cache_key, SimpleEtagCacheKey},
    CacheGetResponse, CacheGetResponseResult, CacheProvider,
};

mod err;
mod get;
mod put;
mod tres_body;

pub use err::*;
pub use get::*;
pub use put::*;
pub use tres_body::*;

pub type ConstLruProviderCacheKey = SimpleEtagCacheKey;

/// Tuple containing the request to the provider and the oneshot
/// sender for the provider to send the response to
pub type ReqTup<ReqBody, ResBody> = (
    ConstLruProviderReq<ReqBody, ResBody>,
    oneshot::Sender<
        Result<ConstLruProviderRes<ReqBody>, ConstLruProviderError<<ResBody as Body>::Error>>,
    >,
);

#[derive(Debug)]
pub enum ConstLruProviderReq<ReqBody, ResBody> {
    Get(http::Request<ReqBody>),
    Put(ConstLruProviderCacheKey, http::Response<ResBody>),
}

#[derive(Debug)]
pub enum ConstLruProviderRes<ReqBody> {
    Get(CacheGetResponse<ReqBody, ConstLruProviderCacheKey>),
    Put(http::Response<ConstLruProviderTResBody>),
}

/// A basic in-memory ConstLru-backed cache provider.
///
/// Meant to be a single instance communicated with using a `tokio::sync::mpsc::channel` via [`ConstLruProviderHandle`]
///
/// Uses [`SimpleEtagCacheKey`] as key type.
///
/// Also stores the `SystemTime` of when the cache entry was created, which serves as the response's
/// last-modified header value
pub struct ConstLruProvider<ReqBody, ResBody: Body, const CAP: usize, I: PrimInt + Unsigned = usize>
{
    const_lru: ConstLru<ConstLruProviderCacheKey, (String, SystemTime), CAP, I>,
    req_rx: mpsc::Receiver<ReqTup<ReqBody, ResBody>>,
}

impl<
        ReqBody: Send + 'static,
        ResBody: Send + Body + 'static,
        const CAP: usize,
        I: PrimInt + Unsigned + Send + 'static,
    > ConstLruProvider<ReqBody, ResBody, CAP, I>
where
    <ResBody as Body>::Data: Send,
    <ResBody as Body>::Error: Error + Send + Sync,
{
    /// Allocates and creates a ConstLruProvider on the heap and returns the [`CacheProvider`] handle to it.
    ///
    /// The ConstLruProvider is dropped once all handles are dropped.
    ///
    /// Should be called once on server init
    ///
    /// `req_buffer` is the size of the `mpsc::channel` connecting [`ConstLruProviderHandle`] to [`ConstLruProvider`]
    pub fn init(req_buffer: usize) -> ConstLruProviderHandle<ReqBody, ResBody> {
        let (req_tx, req_rx) = mpsc::channel(req_buffer);

        let mut this = Self::boxed(req_rx);
        tokio::spawn(async move { this.run().await });

        ConstLruProviderHandle {
            req_tx: PollSender::new(req_tx),
        }
    }

    fn boxed(req_rx: mpsc::Receiver<ReqTup<ReqBody, ResBody>>) -> Box<Self> {
        // directly alloc so that a large ConstLru does not trigger stack overflow
        unsafe {
            let ptr = alloc(Layout::new::<Self>()) as *mut Self;
            let const_lru_ptr = addr_of_mut!((*ptr).const_lru);
            ConstLru::init_at_alloc(const_lru_ptr);
            let req_rx_ptr = addr_of_mut!((*ptr).req_rx);
            req_rx_ptr.write(req_rx);
            Box::from_raw(ptr)
        }
    }

    /// long-running loop
    async fn run(&mut self) {
        while let Some((req, resp_tx)) = self.req_rx.recv().await {
            let res = match req {
                ConstLruProviderReq::Get(req) => {
                    self.on_get_request(req).map(ConstLruProviderRes::Get)
                }
                ConstLruProviderReq::Put(key, resp) => self
                    .on_put_request(key, resp)
                    .await
                    .map(ConstLruProviderRes::Put),
            };
            // ignore error if resp_rx dropped
            let _ = resp_tx.send(res);
        }
        // exits when all req_tx dropped
    }

    fn on_get_request(
        &mut self,
        req: http::Request<ReqBody>,
    ) -> Result<
        CacheGetResponse<ReqBody, ConstLruProviderCacheKey>,
        ConstLruProviderError<ResBody::Error>,
    > {
        let key = calc_simple_etag_cache_key(&req);
        let (cache_etag, last_modified) = match self.const_lru.get(&key) {
            Some(e) => e,
            None => {
                return Ok(CacheGetResponse {
                    req,
                    result: crate::CacheGetResponseResult::Miss(key),
                })
            }
        };
        let if_none_match_iter = req.headers().get_all(IF_NONE_MATCH);
        for etag in if_none_match_iter {
            let etag_str = match etag.to_str() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if etag_str == cache_etag {
                let mut header_map = HeaderMap::new();
                Self::set_response_headers(&mut header_map, etag.clone(), *last_modified);
                return Ok(CacheGetResponse {
                    req,
                    result: CacheGetResponseResult::Hit(header_map),
                });
            }
        }
        Ok(CacheGetResponse {
            req,
            result: CacheGetResponseResult::Miss(key),
        })
    }

    async fn on_put_request(
        &mut self,
        key: ConstLruProviderCacheKey,
        resp: http::Response<ResBody>,
    ) -> Result<http::Response<ConstLruProviderTResBody>, ConstLruProviderError<ResBody::Error>>
    {
        let (mut parts, body) = resp.into_parts();
        let body_bytes = BodyExt::collect(body)
            .await
            .map_err(ConstLruProviderError::ReadResBody)?
            .to_bytes();

        let etag = base64_blake3_body_etag(&body_bytes);
        // unwrap-safety: base64 should always be valid ascii
        let etag_str = etag.to_str().unwrap();

        let curr_val = self
            .const_lru
            .entry(key)
            .or_insert_with(|| (etag_str.to_owned(), SystemTime::now()));

        // don't modify if cached etag is already the same
        if curr_val.0 != etag_str {
            curr_val.0 = etag_str.to_owned();
            curr_val.1 = SystemTime::now();
        }

        let last_modified = curr_val.1;
        Self::set_response_headers(&mut parts.headers, etag, last_modified);

        Ok(http::Response::from_parts(parts, body_bytes.into()))
    }

    fn set_response_headers(
        headers_mut: &mut HeaderMap,
        etag_val: HeaderValue,
        last_modified_val: SystemTime,
    ) {
        headers_mut.append(ETAG, etag_val);
        headers_mut.append(
            CACHE_CONTROL,
            HeaderValue::from_static("max-age=604800,stale-while-revalidate=86400"),
        );
        let last_modified_val = OffsetDateTime::from(last_modified_val)
            .format(&Rfc2822)
            .unwrap();
        headers_mut.append(
            LAST_MODIFIED,
            HeaderValue::from_str(&last_modified_val).unwrap(),
        );
        SimpleEtagCacheKey::set_response_headers(headers_mut);
    }
}

// SERVICE HANDLE

pub struct ConstLruProviderHandle<ReqBody, ResBody: Body> {
    req_tx: PollSender<ReqTup<ReqBody, ResBody>>,
}

impl<ReqBody, ResBody: Body> Clone for ConstLruProviderHandle<ReqBody, ResBody> {
    fn clone(&self) -> Self {
        Self {
            req_tx: self.req_tx.clone(),
        }
    }
}

impl<ReqBody: Send, ResBody: Body + Send> CacheProvider<ReqBody, ResBody>
    for ConstLruProviderHandle<ReqBody, ResBody>
where
    ResBody::Error: Send,
{
    type Key = ConstLruProviderCacheKey;
    type TResBody = ConstLruProviderTResBody;
}
