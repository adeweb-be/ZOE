//! The `zoe serve` process. A tokio server that speaks the ZEO 6 msgpack
//! protocol, so that an unmodified `ZEO.ClientStorage` connects. It is the
//! only crate that uses tokio. It has no Python inside.

mod protocol;
