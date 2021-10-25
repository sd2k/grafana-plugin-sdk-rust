//! Resource services, which allow backend plugins to handle custom HTTP requests and responses.
use std::{
    convert::{TryFrom, TryInto},
    pin::Pin,
};

use futures_util::StreamExt;
use http::{
    header::{HeaderName, HeaderValue},
    Request, Response,
};
use itertools::Itertools;

use crate::{
    backend::{ConversionError, PluginContext},
    pluginv2,
};

/// A request for a resource call.
#[derive(Debug)]
pub struct CallResourceRequest<T = Vec<u8>> {
    /// Details of the plugin instance from which the request originated.
    pub plugin_context: Option<PluginContext>,
    /// The HTTP request.
    pub request: Request<T>,
}

impl TryFrom<pluginv2::CallResourceRequest> for CallResourceRequest {
    type Error = ConversionError;
    fn try_from(other: pluginv2::CallResourceRequest) -> Result<Self, Self::Error> {
        let mut request = Request::builder();
        {
            let headers = request
                .headers_mut()
                .expect("Request should always be valid at this point");
            for (k, vs) in other.headers.iter() {
                let header_name =
                    k.parse::<HeaderName>()
                        .map_err(|source| ConversionError::InvalidRequest {
                            source: source.into(),
                        })?;
                for v in &vs.values {
                    let header_val = v.parse::<HeaderValue>().map_err(|source| {
                        ConversionError::InvalidRequest {
                            source: source.into(),
                        }
                    })?;
                    headers.append(header_name.clone(), header_val);
                }
            }
        }
        Ok(Self {
            plugin_context: other.plugin_context.map(TryInto::try_into).transpose()?,
            request: request
                .method(other.method.as_str())
                .uri(other.url)
                .body(other.body)
                .map_err(|source| ConversionError::InvalidRequest { source })?,
        })
    }
}

impl TryFrom<Response<Vec<u8>>> for pluginv2::CallResourceResponse {
    type Error = ConversionError;
    fn try_from(mut other: Response<Vec<u8>>) -> Result<Self, Self::Error> {
        let grouped_headers = other.headers_mut().drain().group_by(|x| x.0.clone());
        let headers = grouped_headers
            .into_iter()
            .map(|(k, values)| {
                Ok((
                    k.as_ref()
                        .map(|h| h.to_string())
                        .ok_or(ConversionError::InvalidResponse)?,
                    pluginv2::StringList {
                        values: values
                            .map(|v| {
                                v.1.to_str()
                                    .map(|s| s.to_string())
                                    .map_err(|_| ConversionError::InvalidResponse)
                            })
                            .collect::<Result<_, _>>()?,
                    },
                ))
            })
            .collect::<Result<_, _>>()?;
        drop(grouped_headers);
        Ok(Self {
            code: other.status().as_u16().into(),
            headers,
            body: other.into_body(),
        })
    }
}

fn body_to_response(body: Vec<u8>) -> pluginv2::CallResourceResponse {
    pluginv2::CallResourceResponse {
        code: 200,
        headers: std::collections::HashMap::new(),
        body,
    }
}

/// Type alias for a pinned, boxed future with a fallible HTTP response as output, with a custom error type.
pub type BoxResourceFuture<E> = Pin<
    Box<dyn std::future::Future<Output = Result<Response<Vec<u8>>, E>> + Send + Sync + 'static>,
>;

/// Type alias for a pinned, boxed stream of HTTP responses with a custom error type.
pub type BoxResourceStream<E> =
    Pin<Box<dyn futures_core::Stream<Item = Result<Vec<u8>, E>> + Send + Sync + 'static>>;

/// Trait for plugins that can handle arbitrary resource requests.
///
/// Implementing this trait allows plugins to handle a wide variety of use cases beyond
/// 'just' responding to requests for data and returning dataframes.
///
/// See <https://grafana.com/docs/grafana/latest/developers/plugins/backend/#resources> for
/// some examples of how this can be used.
///
/// # Example
///
/// The following shows an example implementation of [`ResourceService`] which handles
/// two endpoints:
/// - /echo, which echos back the request's URL, headers and body in three responses,
/// - /count, which increments the plugin's internal count and returns it in a response.
///
/// ```rust
/// use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
///
/// use async_stream::stream;
/// use grafana_plugin_sdk::backend;
/// use http::Response;
///
/// struct Plugin(Arc<AtomicUsize>);
///
/// impl Plugin {
///     // Increment the counter and return the stringified result in a `Response`.
///     fn inc_and_respond(&self) -> Response<Vec<u8>> {
///         Response::new(
///             self.0
///                 .fetch_add(1, Ordering::SeqCst)
///                 .to_string()
///                 .into_bytes()
///         )
///     }
/// }
///
/// #[backend::async_trait]
/// impl backend::ResourceService for Plugin {
///     type Error = http::Error;
///     type Stream = backend::BoxResourceStream<Self::Error>;
///     async fn call_resource(&self, r: backend::CallResourceRequest) -> (Result<http::Response<Vec<u8>>, Self::Error>, Self::Stream) {
///         let count = Arc::clone(&self.0);
///         let (response, stream): (_, Self::Stream) = match r.request.uri().path() {
///             // Just send back a single response.
///             "/echo" => (
///                 Ok(Response::new(r.request.into_body())),
///                 Box::pin(futures::stream::empty()),
///             ),
///             // Send an initial response with the current count, then stream the gradually
///             // incrementing count back to the client.
///             "/count" => (
///                 Ok(Response::new(
///                     count
///                         .fetch_add(1, Ordering::SeqCst)
///                         .to_string()
///                         .into_bytes(),
///                 )),
///                 Box::pin(async_stream::try_stream! {
///                     loop {
///                         let body = count
///                             .fetch_add(1, Ordering::SeqCst)
///                             .to_string()
///                             .into_bytes();
///                         yield body;
///                     }
///                 }),
///             ),
///             _ => (
///                 Response::builder().status(404).body(vec![]),
///                 Box::pin(futures::stream::empty()),
///             ),
///         };
///         (response, stream)
///     }
/// }
/// ```
#[tonic::async_trait]
pub trait ResourceService {
    /// The error type that can be returned by individual responses.
    type Error: std::error::Error;

    /// The type of stream of optional additional data returned by `run_stream`.
    ///
    /// This will generally be impossible to name directly, so returning the
    /// [`BoxResourceStream`] type alias will probably be more convenient.
    type Stream: futures_core::Stream<Item = Result<Vec<u8>, Self::Error>> + Send + Sync;

    /// Handle a resource request.
    ///
    /// It is completely up to the implementor how to handle the incoming request.
    ///
    /// A stream of responses can be returned. A simple way to return just a single response
    /// is to use `futures_util::stream::once`.
    async fn call_resource(
        &self,
        request: CallResourceRequest,
    ) -> (Result<http::Response<Vec<u8>>, Self::Error>, Self::Stream);
}

#[tonic::async_trait]
impl<T> pluginv2::resource_server::Resource for T
where
    T: ResourceService + Send + Sync + 'static,
{
    type CallResourceStream = Pin<
        Box<
            dyn futures_core::Stream<Item = Result<pluginv2::CallResourceResponse, tonic::Status>>
                + Send
                + Sync,
        >,
    >;
    #[tracing::instrument(skip(self), level = "debug")]
    async fn call_resource(
        &self,
        request: tonic::Request<pluginv2::CallResourceRequest>,
    ) -> Result<tonic::Response<Self::CallResourceStream>, tonic::Status> {
        let request = request
            .into_inner()
            .try_into()
            .map_err(ConversionError::into_tonic_status)?;
        let (initial_response, stream) = ResourceService::call_resource(self, request).await;
        let initial_response_converted = initial_response
            .map_err(|e| tonic::Status::internal(e.to_string()))
            .and_then(|response| {
                pluginv2::CallResourceResponse::try_from(response)
                    .map_err(|e| tonic::Status::internal(e.to_string()))
            });
        let stream =
            futures_util::stream::once(futures_util::future::ready(initial_response_converted))
                .chain(stream.map(|response| {
                    response
                        .map(body_to_response)
                        .map_err(|e| tonic::Status::internal(e.to_string()))
                }));
        Ok(tonic::Response::new(Box::pin(stream)))
    }
}
