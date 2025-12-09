pub mod base;
pub mod luau;
pub mod parallelluau;
#[cfg(feature = "deno")]
pub mod denobridge;

pub const MAX_PROXY_DEPTH: usize = 10;
