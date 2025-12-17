import * as textEncoding from "./00_text_encoding.js";
import * as b64 from "./01_base64.js";

globalThis.decode = textEncoding.decode;
globalThis.TextDecoder = textEncoding.TextDecoder;
globalThis.TextDecoderStream = textEncoding.TextDecoderStream;
globalThis.TextEncoder = textEncoding.TextEncoder;
globalThis.TextEncoderStream = textEncoding.TextEncoderStream;
globalThis.atob = b64.atob;
globalThis.btoa = b64.btoa;