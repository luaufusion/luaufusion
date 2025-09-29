use deno_core::Extension;
use deno_core::extension;

pub(crate) trait ExtensionTrait<A> {
    fn init(options: A) -> Extension;

    // Clears the js and esm files for warmup to avoid reloading them
    fn for_warmup(mut ext: Extension) -> Extension {
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
impl ExtensionTrait<()> for deno_console::deno_console {
    fn init((): ()) -> Extension {
        deno_console::deno_console::init()
    }
}

pub(crate) fn console_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        deno_console::deno_console::build((), is_snapshot),
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
impl ExtensionTrait<()> for deno_url::deno_url {
    fn init((): ()) -> Extension {
        deno_url::deno_url::init()
    }
}

pub(crate) fn url_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        deno_url::deno_url::build((), is_snapshot),
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
impl ExtensionTrait<()> for deno_webidl::deno_webidl {
    fn init((): ()) -> Extension {
        deno_webidl::deno_webidl::init()
    }
}

pub(crate) fn webidl_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![
        deno_webidl::deno_webidl::build((), is_snapshot),
        init_webidl::build((), is_snapshot),
    ]
}

extension!(
    init_b64,
    ops = [
        super::base64_ops::op_base64_decode, super::base64_ops::op_base64_atob, super::base64_ops::op_base64_encode, super::base64_ops::op_base64_btoa,
    ],
    esm_entry_point = "ext:init_b64/base64.js",
    esm = [ dir "src/denobridge/denoexts", "base64.js" ],
);
impl ExtensionTrait<()> for init_b64 {
    fn init((): ()) -> Extension {
        init_b64::init()
    }
}

pub(crate) fn b64_extensions(is_snapshot: bool) -> Vec<Extension> {
    vec![init_b64::build((), is_snapshot)]
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
    ops = [
        crate::denobridge::__dropluaobject,
        crate::denobridge::__luadispatch,
        crate::denobridge::__luarun,
        crate::denobridge::__luaret,
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
    vec![deno_structuredclone::build((), is_snapshot)]
}

pub(crate) fn all_extensions(is_snapshot: bool) -> Vec<Extension> {
    let mut exts = vec![];
    exts.extend(console_extensions(is_snapshot));
    exts.extend(webidl_extensions(is_snapshot));
    exts.extend(url_extensions(is_snapshot));
    exts.extend(b64_extensions(is_snapshot));
    exts.extend(structuredclone_extensions(is_snapshot));
    exts.extend(luau_bridge_extension(is_snapshot));
    exts
}

