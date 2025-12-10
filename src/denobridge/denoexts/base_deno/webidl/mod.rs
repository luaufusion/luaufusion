deno_core::extension!(
    deno_webidl, 
    esm = [dir "src/denobridge/denoexts/base_deno/webidl/", "00_webidl.js"],
);