#![deny(unsafe_code)]

pub mod cache;
pub mod client;
pub mod decompress;
pub mod dns_tls;
pub mod error;
pub mod resource;
pub mod timing;

pub use cache::{CacheControlDirectives, HttpCache, HttpCacheEntry, HttpCacheMeta};
pub use client::{
    parse_cookie_header, BodyStream, ConditionalRequest, H3Sender, HttpClient, HttpResponse,
    RequestBodyStream, StreamingResponse,
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
