import { core, primordials } from "ext:core/mod.js"
const { ObjectDefineProperties, ObjectSetPrototypeOf } = primordials;

import * as textEncoding from "./00_text_encoding.js";
import * as b64 from "./01_base64.js";
import * as domexceptions from "./01_dom_exception.js";
import * as events from "./02_events.js";
import * as globalInterfaces from "./03_global_interfaces.js";
import * as globalScope from "./98_global_scope_window.js";

globalThis.decode = textEncoding.decode;
globalThis.TextDecoder = textEncoding.TextDecoder;
globalThis.TextDecoderStream = textEncoding.TextDecoderStream;
globalThis.TextEncoder = textEncoding.TextEncoder;
globalThis.TextEncoderStream = textEncoding.TextEncoderStream;

// base64.js
globalThis.atob = b64.atob;
globalThis.btoa = b64.btoa;

// dom dom.js
globalThis.DOMException = domexceptions.DOMException;

// events.js
globalThis.CloseEvent = events.CloseEvent;
globalThis.CustomEvent = events.CustomEvent;
globalThis.defineEventHandler = events.defineEventHandler;
globalThis.dispatch = events.dispatch;
globalThis.ErrorEvent = events.ErrorEvent;
globalThis.Event = events.Event;
globalThis.EventTarget = events.EventTarget;
globalThis.EventTargetPrototype = events.EventTargetPrototype;
globalThis.listenerCount = events.listenerCount;
globalThis.MessageEvent = events.MessageEvent;
globalThis.ProgressEvent = events.ProgressEvent;
globalThis.PromiseRejectionEvent = events.PromiseRejectionEvent;
globalThis.reportError = events.reportError;
globalThis.reportException = events.reportException;
globalThis.saveGlobalThisReference = events.saveGlobalThisReference;
globalThis.setEventTargetData = events.setEventTargetData;
globalThis.setIsTrusted = events.setIsTrusted;
globalThis.setTarget = events.setTarget;

// globalInterfaces

let globalThis_;
globalThis_ = globalThis;

events.setEventTargetData(globalThis);
events.saveGlobalThisReference(globalThis);

// Nothing listens to this, but it warms up the code paths for event dispatch
(new events.EventTarget()).dispatchEvent(new events.Event("warmup"));

globalThis.Window = globalInterfaces.Window;

ObjectDefineProperties(globalThis, globalScope.mainRuntimeGlobalProperties);
ObjectSetPrototypeOf(globalThis, Window.prototype);

events.defineEventHandler(globalThis, "error");
events.defineEventHandler(globalThis, "load");
events.defineEventHandler(globalThis, "beforeunload");
events.defineEventHandler(globalThis, "unload")