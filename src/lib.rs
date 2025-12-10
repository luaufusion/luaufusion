pub mod base;
pub mod luau;
#[cfg(feature = "parallel_luau")]
pub mod parallelluau;
#[cfg(feature = "deno")]
pub mod denobridge;

pub const MAX_PROXY_DEPTH: usize = 10;
