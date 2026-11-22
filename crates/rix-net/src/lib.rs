#![deny(unsafe_code)]

pub mod cache;
pub mod client;
pub mod cors;
pub mod decompress;
pub mod dns_tls;
pub mod error;
pub mod resource;
pub mod timing;
pub mod websocket;

pub use cache::{CacheControlDirectives, HttpCache, HttpCacheEntry, HttpCacheMeta};
pub use client::{
    parse_cookie_header, BodyStream, ConditionalRequest, H3Sender, HttpClient, HttpResponse,
    RequestBodyStream, StreamingResponse,
};
pub use cors::{
    filter_cors_response_headers, is_cors_safelisted_method, is_cors_safelisted_request_header,
    validate_cors_response, AbortHandle, FetchCredentials, FetchEngine, FetchMode, FetchRedirect,
    FetchRequest, FetchResponse, FetchResponseType, PreflightCache,
};
pub use decompress::{
    decompress_body, decompress_body_for_header, encodings_from_header, ContentEncoding,
};
pub use dns_tls::{DnsResolver, NegotiatedProtocol, TlsManager, ALPN_PROTOCOLS};
pub use error::{NetError, Result};
pub use resource::{
    sniff_mime_type, sniff_mime_type_with_name, ChromeAssetProvider, DiskCache, Resource,
    ResourceManager, ResourcePriority,
};
pub use timing::NavigationTiming;
pub use websocket::{
    compute_ws_accept_key, encode_client_frame, encode_server_frame, WebSocketConnection,
    WsCommand, WsMessage,
};
