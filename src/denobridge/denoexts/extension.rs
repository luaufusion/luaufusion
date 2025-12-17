use deno_core::Extension;
use deno_core::extension;

use crate::denobridge::denoexts::base_deno::web::deno_web;

pub(crate) trait ExtensionTrait<A> {
    fn init(options: A) -> Extension;

    // Clears the js and esm files for warmup to avoid reloading them
    fn for_warmup(mut ext: Extension) -> Extension {
        ext.op_state_fn = None;
        ext.js_files = ::std::borrow::Cow::Borrowed(&[]);
        ext.esm_files = ::std::borrow::Cow::Borrowed(&[]);
        ext.esm_entry_point = ::std::option::Option::None;

        ext
    }

    /// Builds an extension
    fn build(options: A, is_snapshot: bool) -> Extension {
        let ext: Extension = Self::init(options);
        if is_snapshot {
            Self::for_warmup(ext)
        } else {
            ext
        }
    }
}

extension!(
    init_console,
    deps = [],
    esm_entry_point = "ext:init_console/console.js",
    esm = [ dir "src/denobridge/denoexts", "console.js" ],
);
impl ExtensionTrait<()> for init_console {
    fn init((): ()) -> Extension {
        init_console::init()
    }
}
impl ExtensionTrait<()> for super::base_deno::console::deno_console {
    fn init((): ()) -> Extension {
        super::base_deno::console::deno_console::init()
    }
}

pub(crate) fn console_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        super::base_deno::console::deno_console::build((), is_snapshot),
        init_console::build((), is_snapshot),
    ]
}

extension!(
    init_url,
    deps = [],
    esm_entry_point = "ext:init_url/url.js",
    esm = [ dir "src/denobridge/denoexts", "url.js" ],
);
impl ExtensionTrait<()> for init_url {
    fn init((): ()) -> Extension {
        init_url::init()
    }
}
impl ExtensionTrait<()> for super::base_deno::url::deno_url {
    fn init((): ()) -> Extension {
        super::base_deno::url::deno_url::init()
    }
}

pub(crate) fn url_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        super::base_deno::url::deno_url::build((), is_snapshot),
        init_url::build((), is_snapshot),
    ]
}

extension!(
    init_webidl,
    deps = [],
    esm_entry_point = "ext:init_webidl/webidl.js",
    esm = [ dir "src/denobridge/denoexts", "webidl.js" ],
);
impl ExtensionTrait<()> for init_webidl {
    fn init((): ()) -> Extension {
        init_webidl::init()
    }
}
impl ExtensionTrait<()> for super::base_deno::webidl::deno_webidl {
    fn init((): ()) -> Extension {
        super::base_deno::webidl::deno_webidl::init()
    }
}

pub(crate) fn webidl_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        super::base_deno::webidl::deno_webidl::build((), is_snapshot),
        init_webidl::build((), is_snapshot),
    ]
}

impl ExtensionTrait<()> for deno_web {
    fn init((): ()) -> Extension {
        deno_web::init()
    }
}

pub(crate) fn web_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![deno_web::build((), is_snapshot)]
}

extension!(
    deno_structuredclone,
    esm_entry_point = "ext:deno_structuredclone/structuredclone.js",
    esm = [ dir "src/denobridge/denoexts", "structuredclone.js" ],
);
impl ExtensionTrait<()> for deno_structuredclone {
    fn init((): ()) -> Extension {
        deno_structuredclone::init()
    }
}

pub(crate) fn structuredclone_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![deno_structuredclone::build((), is_snapshot)]
}

extension!(
    luau_bridge,
    objects = [
        crate::denobridge::event::EventBridge,
    ],
    esm_entry_point = "ext:luau_bridge/cls.js",
    esm = [ dir "src/denobridge/bridge", "cls.js" ],
);
impl ExtensionTrait<()> for luau_bridge {
    fn init((): ()) -> Extension {
        luau_bridge::init()
    }
}

pub(crate) fn luau_bridge_extension(is_snapshot: bool) -> Vec<Extension> {
    vec![luau_bridge::build((), is_snapshot)]
}

pub(crate) fn all_extensions(is_snapshot: bool) -> Vec<Extension> {
    let mut exts = vec![];
    exts.extend(console_extensions(is_snapshot));
    exts.extend(webidl_extensions(is_snapshot));
    exts.extend(web_extensions(is_snapshot));
    exts.extend(url_extensions(is_snapshot));
    exts.extend(structuredclone_extensions(is_snapshot));
    exts.extend(luau_bridge_extension(is_snapshot));
    exts
}

