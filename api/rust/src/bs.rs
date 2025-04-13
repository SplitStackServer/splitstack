include!(concat!(env!("OUT_DIR"), "/bs/bs.rs"));
#[cfg(feature = "json")]
include!(concat!(env!("OUT_DIR"), "/bs/bs.serde.rs"));
