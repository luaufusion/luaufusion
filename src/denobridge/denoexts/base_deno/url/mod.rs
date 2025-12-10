mod url;
mod urlpattern;

deno_core::extension!(
  deno_url,
  deps = [deno_webidl],
  ops = [
    url::op_url_reparse,
    url::op_url_parse,
    url::op_url_get_serialization,
    url::op_url_parse_with_base,
    url::op_url_parse_search_params,
    url::op_url_stringify_search_params,
    urlpattern::op_urlpattern_parse,
    urlpattern::op_urlpattern_process_match_input
  ],
  esm = [dir "src/denobridge/denoexts/base_deno/url/", "00_url.js", "01_urlpattern.js"],
);
