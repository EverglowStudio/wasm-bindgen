//! This crate contains implementation APIs for the `#[wasm_bindgen]` attribute.

#![doc(html_root_url = "https://docs.rs/wasm-bindgen-macro-support/0.2")]

#[macro_use]
mod error;

mod ast;
mod codegen;
mod encode;
mod generics;
mod hash;
mod parser;

use codegen::TryToTokens;
use error::Diagnostic;
pub use parser::BindgenAttrs;
use parser::{ConvertToAst, MacroParse};
use proc_macro2::TokenStream;
use quote::quote;
use quote::ToTokens;
use quote::TokenStreamExt;
use std::collections::BTreeSet;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use syn::parse::{Parse, ParseStream, Result as SynResult};
use syn::Token;

/// Complete, explicit Cargo/rustc context for a programmatic macro expansion.
///
/// The regular proc-macro entry points construct this value from Cargo's
/// environment.  Library callers must construct it explicitly, which prevents
/// one in-process build from inheriting another build's package identity,
/// target, cfg set, or local-module root.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ExpansionContext {
    manifest_dir: PathBuf,
    crate_name: String,
    crate_version: String,
    cfg: BTreeSet<String>,
    target: String,
}

impl ExpansionContext {
    pub fn new(
        manifest_dir: impl Into<PathBuf>,
        crate_name: impl Into<String>,
        crate_version: impl Into<String>,
        cfg: impl IntoIterator<Item = String>,
        target: impl Into<String>,
    ) -> Result<Self, Diagnostic> {
        let manifest_dir = manifest_dir.into();
        let crate_name = crate_name.into();
        let crate_version = crate_version.into();
        let target = target.into();
        if manifest_dir.as_os_str().is_empty() {
            return Err(Diagnostic::error(
                "expansion manifest_dir must not be empty",
            ));
        }
        for (role, value) in [
            ("crate_name", crate_name.as_str()),
            ("crate_version", crate_version.as_str()),
            ("target", target.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(Diagnostic::error(format!(
                    "expansion {role} must not be empty"
                )));
            }
        }
        Ok(Self {
            manifest_dir,
            crate_name,
            crate_version,
            cfg: cfg.into_iter().collect(),
            target,
        })
    }

    /// Build the context used by the existing proc-macro entry points.
    pub fn from_cargo_env() -> Result<Self, Diagnostic> {
        fn required(name: &'static str) -> Result<String, Diagnostic> {
            env::var(name).map_err(|_| Diagnostic::error(format!("should have {name} env var")))
        }

        let manifest_dir = required("CARGO_MANIFEST_DIR")?;
        let crate_name = required("CARGO_PKG_NAME")?;
        let crate_version = required("CARGO_PKG_VERSION")?;
        let target = env::var("TARGET")
            .or_else(|_| env::var("CARGO_BUILD_TARGET"))
            .or_else(|_| env::var("CARGO_CFG_TARGET_ARCH"))
            .unwrap_or_else(|_| "cargo-selected-target".to_owned());
        let cfg = env::vars()
            .filter_map(|(name, value)| {
                name.strip_prefix("CARGO_CFG_")
                    .map(|name| format!("{name}={value}"))
            })
            .chain(cfg!(wasm_bindgen_use_js_sys).then(|| "wasm_bindgen_use_js_sys".to_owned()))
            .chain(
                env::var_os("WASM_BINDGEN_USE_JS_SYS")
                    .is_some()
                    .then(|| "wasm_bindgen_use_js_sys".to_owned()),
            );
        Self::new(manifest_dir, crate_name, crate_version, cfg, target)
    }

    pub fn manifest_dir(&self) -> &Path {
        &self.manifest_dir
    }

    pub fn crate_name(&self) -> &str {
        &self.crate_name
    }

    pub fn crate_version(&self) -> &str {
        &self.crate_version
    }

    pub fn cfg(&self) -> &BTreeSet<String> {
        &self.cfg
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn symbol_salt(&self) -> usize {
        // Preserve the proc-macro's historical crate salt exactly. Target/cfg
        // still drive expansion decisions through this explicit context, but
        // do not churn otherwise identical internal symbol names.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.crate_name.hash(&mut hasher);
        self.crate_version.hash(&mut hasher);
        hasher.finish() as usize
    }

    pub(crate) fn use_js_sys_futures(&self) -> bool {
        self.cfg.contains("wasm_bindgen_use_js_sys")
    }
}

/// Reusable, environment-independent macro expansion entry point.
#[derive(Clone, Debug)]
pub struct ExpansionBuilder {
    context: ExpansionContext,
}

impl ExpansionBuilder {
    pub fn new(context: ExpansionContext) -> Self {
        Self { context }
    }

    pub fn context(&self) -> &ExpansionContext {
        &self.context
    }

    pub fn expand(&self, attr: TokenStream, input: TokenStream) -> Result<TokenStream, Diagnostic> {
        // Descriptor de-duplication is intentionally retained for normal
        // proc-macro invocations in one crate.  Programmatic builds get a
        // fresh, bounded scope so consecutive invocations cannot influence
        // each other; independent threads are isolated by the underlying TLS.
        codegen::reset_descriptors_emitted();
        let result = expand_in_context(self.context.clone(), attr, input);
        codegen::reset_descriptors_emitted();
        result
    }
}

/// Takes the parsed input from a `#[wasm_bindgen]` macro and returns the generated bindings
pub fn expand(attr: TokenStream, input: TokenStream) -> Result<TokenStream, Diagnostic> {
    expand_in_context(ExpansionContext::from_cargo_env()?, attr, input)
}

fn expand_in_context(
    context: ExpansionContext,
    attr: TokenStream,
    input: TokenStream,
) -> Result<TokenStream, Diagnostic> {
    parser::reset_attrs_used();
    // if struct is encountered, add `derive` attribute and let everything happen there (workaround
    // to help parsing cfg_attr correctly).
    let item = syn::parse2::<syn::Item>(input)?;
    if let syn::Item::Struct(mut s) = item {
        let opts: BindgenAttrs = syn::parse2(attr.clone())?;
        let wasm_bindgen = opts
            .wasm_bindgen()
            .cloned()
            .unwrap_or_else(|| syn::parse_quote! { ::wasm_bindgen });

        // Inject `parent: <wasm_bindgen>::Parent<Parent>` when the struct
        // declares `#[wasm_bindgen(extends = Parent)]`, so users never write
        // the field themselves. Also rejects user-declared `Parent<T>` fields.
        let extends_path = opts.attrs.iter().find_map(|(_, a)| match a {
            parser::BindgenAttr::Extends(_, path) => Some(path.clone()),
            _ => None,
        });
        parser::inject_parent_field(&mut s, extends_path.as_ref(), &wasm_bindgen)?;

        let item = quote! {
            #[derive(#wasm_bindgen::__rt::BindgenedStruct)]
            #[wasm_bindgen(#attr)]
            #s
        };
        return Ok(item);
    }

    let opts = syn::parse2(attr)?;
    let mut tokens = proc_macro2::TokenStream::new();
    let mut program = ast::Program::new(context);
    item.macro_parse(&mut program, (Some(opts), &mut tokens))?;
    program.try_to_tokens(&mut tokens)?;

    // If we successfully got here then we should have used up all attributes
    // and considered all of them to see if they were used. If one was forgotten
    // that's a bug on our end, so sanity check here.
    parser::check_unused_attrs(&mut tokens);

    Ok(tokens)
}

/// Takes the parsed input from a `wasm_bindgen::link_to` macro and returns the generated link
pub fn expand_link_to(input: TokenStream) -> Result<TokenStream, Diagnostic> {
    parser::reset_attrs_used();
    let opts = syn::parse2(input)?;

    let mut tokens = proc_macro2::TokenStream::new();
    let link = parser::link_to(opts, ExpansionContext::from_cargo_env()?)?;
    link.try_to_tokens(&mut tokens)?;

    Ok(tokens)
}

/// Takes the parsed input from a `#[wasm_bindgen]` macro and returns the generated bindings
pub fn expand_class_marker(
    attr: TokenStream,
    input: TokenStream,
) -> Result<TokenStream, Diagnostic> {
    parser::reset_attrs_used();
    let mut item = syn::parse2::<syn::ImplItemFn>(input)?;
    let opts: ClassMarker = syn::parse2(attr)?;

    let mut program = ast::Program::new(ExpansionContext::from_cargo_env()?);
    item.macro_parse(&mut program, &opts)?;

    // This is where things are slightly different, we are being expanded in the
    // context of an impl so we can't inject arbitrary item-like tokens into the
    // output stream. If we were to do that then it wouldn't parse!
    //
    // Instead what we want to do is to generate the tokens for `program` into
    // the header of the function. This'll inject some no_mangle functions and
    // statics and such, and they should all be valid in the context of the
    // start of a function.
    //
    // We manually implement `ToTokens for ImplItemFn` here, injecting our
    // program's tokens before the actual method's inner body tokens.
    let mut tokens = proc_macro2::TokenStream::new();
    tokens.append_all(
        item.attrs
            .iter()
            .filter(|attr| matches!(attr.style, syn::AttrStyle::Outer)),
    );
    item.vis.to_tokens(&mut tokens);
    item.sig.to_tokens(&mut tokens);
    let mut err = None;
    item.block.brace_token.surround(&mut tokens, |tokens| {
        if let Err(e) = program.try_to_tokens(tokens) {
            err = Some(e);
        }
        parser::check_unused_attrs(tokens); // same as above
        tokens.append_all(
            item.attrs
                .iter()
                .filter(|attr| matches!(attr.style, syn::AttrStyle::Inner(_))),
        );
        tokens.append_all(&item.block.stmts);
    });

    if let Some(err) = err {
        return Err(err);
    }

    Ok(tokens)
}

struct ClassMarker {
    class: syn::Ident,
    js_class: String,
    js_namespace: Option<Vec<String>>,
    wasm_bindgen: syn::Path,
    wasm_bindgen_futures: syn::Path,
    js_sys: syn::Path,
}

impl Parse for ClassMarker {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let class = input.parse::<syn::Ident>()?;
        input.parse::<Token![=]>()?;
        let mut js_class = input.parse::<syn::LitStr>()?.value();
        js_class = js_class
            .strip_prefix("r#")
            .map(String::from)
            .unwrap_or(js_class);

        let mut js_namespace: Option<Vec<String>> = None;
        let mut wasm_bindgen = None;
        let mut wasm_bindgen_futures = None;
        let mut js_sys = None;

        loop {
            if input.parse::<Option<Token![,]>>()?.is_some() {
                let ident = input.parse::<syn::Ident>()?;

                if ident == "js_namespace" {
                    if js_namespace.is_some() {
                        return Err(syn::Error::new(
                            ident.span(),
                            "found duplicate `js_namespace`",
                        ));
                    }
                    input.parse::<Token![=]>()?;
                    let content;
                    syn::bracketed!(content in input);
                    let segs: syn::punctuated::Punctuated<syn::LitStr, Token![,]> = content
                        .parse_terminated(|p: ParseStream| p.parse::<syn::LitStr>(), Token![,])?;
                    js_namespace = Some(segs.into_iter().map(|s| s.value()).collect());
                } else if ident == "wasm_bindgen" {
                    if wasm_bindgen.is_some() {
                        return Err(syn::Error::new(
                            ident.span(),
                            "found duplicate `wasm_bindgen`",
                        ));
                    }

                    input.parse::<Token![=]>()?;
                    wasm_bindgen = Some(input.parse::<syn::Path>()?);
                } else if ident == "wasm_bindgen_futures" {
                    if wasm_bindgen_futures.is_some() {
                        return Err(syn::Error::new(
                            ident.span(),
                            "found duplicate `wasm_bindgen_futures`",
                        ));
                    }

                    input.parse::<Token![=]>()?;
                    wasm_bindgen_futures = Some(input.parse::<syn::Path>()?);
                } else if ident == "js_sys" {
                    if js_sys.is_some() {
                        return Err(syn::Error::new(ident.span(), "found duplicate `js_sys`"));
                    }

                    input.parse::<Token![=]>()?;
                    js_sys = Some(input.parse::<syn::Path>()?);
                } else {
                    return Err(syn::Error::new(
                        ident.span(),
                        "expected `js_namespace`, `wasm_bindgen`, `wasm_bindgen_futures`, or `js_sys`",
                    ));
                }
            } else {
                break;
            }
        }

        Ok(ClassMarker {
            class,
            js_class,
            js_namespace,
            wasm_bindgen: wasm_bindgen.unwrap_or_else(|| syn::parse_quote! { wasm_bindgen }),
            wasm_bindgen_futures: wasm_bindgen_futures
                .unwrap_or_else(|| syn::parse_quote! { wasm_bindgen_futures }),
            js_sys: js_sys.unwrap_or_else(|| syn::parse_quote! { js_sys }),
        })
    }
}

#[cfg(test)]
mod programmatic_tests {
    use super::*;

    fn context(crate_name: &str) -> ExpansionContext {
        ExpansionContext::new(
            std::env::current_dir().unwrap(),
            crate_name,
            "1.2.3",
            ["target_feature=reference-types".to_owned()],
            "wasm32-unknown-unknown",
        )
        .unwrap()
    }

    fn expand(builder: &ExpansionBuilder) -> String {
        let item = quote::quote! {
            extern "C" {
                fn imported_value(value: i32) -> i32;
            }
        };
        builder
            .expand(TokenStream::new(), item)
            .unwrap()
            .to_string()
    }

    fn expand_async(builder: &ExpansionBuilder) -> String {
        let item = quote::quote! {
            pub async fn exported_value(value: i32) -> i32 {
                value + 1
            }
        };
        builder
            .expand(TokenStream::new(), item)
            .unwrap()
            .to_string()
    }

    #[test]
    fn explicit_context_is_deterministic_and_isolated_between_builds() {
        let builder = ExpansionBuilder::new(context("programmatic_fixture"));
        let first = expand(&builder);
        let second = expand(&builder);
        assert_eq!(first, second);
        assert!(first.contains("__wbindgen_describe"));

        let other = expand(&ExpansionBuilder::new(context("other_fixture")));
        assert_ne!(first, other, "crate identity must salt generated symbols");
    }

    #[test]
    fn explicit_context_is_deterministic_across_parallel_builds() {
        let expected = expand(&ExpansionBuilder::new(context("parallel_fixture")));
        let builds = (0..8)
            .map(|_| {
                std::thread::spawn(|| expand(&ExpansionBuilder::new(context("parallel_fixture"))))
            })
            .collect::<Vec<_>>();
        for build in builds {
            assert_eq!(build.join().unwrap(), expected);
        }
    }

    #[test]
    fn explicit_cfg_controls_async_output_without_ambient_input() {
        struct RestoreEnvironment {
            use_js_sys: Option<std::ffi::OsString>,
            rustflags: Option<std::ffi::OsString>,
        }

        impl Drop for RestoreEnvironment {
            fn drop(&mut self) {
                match self.use_js_sys.take() {
                    Some(value) => std::env::set_var("WASM_BINDGEN_USE_JS_SYS", value),
                    None => std::env::remove_var("WASM_BINDGEN_USE_JS_SYS"),
                }
                match self.rustflags.take() {
                    Some(value) => std::env::set_var("RUSTFLAGS", value),
                    None => std::env::remove_var("RUSTFLAGS"),
                }
            }
        }

        let disabled = ExpansionContext::new(
            std::env::current_dir().unwrap(),
            "explicit_cfg",
            "1.2.3",
            Vec::<String>::new(),
            "wasm32-unknown-unknown",
        )
        .unwrap();
        let enabled = ExpansionContext::new(
            std::env::current_dir().unwrap(),
            "explicit_cfg",
            "1.2.3",
            ["wasm_bindgen_use_js_sys".to_owned()],
            "wasm32-unknown-unknown",
        )
        .unwrap();

        let disabled_output = expand_async(&ExpansionBuilder::new(disabled.clone()));
        let enabled_output = expand_async(&ExpansionBuilder::new(enabled));
        assert!(disabled_output.contains("wasm_bindgen_futures"));
        assert!(enabled_output.contains("js_sys :: futures"));
        assert_ne!(disabled_output, enabled_output);

        let _restore = RestoreEnvironment {
            use_js_sys: std::env::var_os("WASM_BINDGEN_USE_JS_SYS"),
            rustflags: std::env::var_os("RUSTFLAGS"),
        };
        std::env::set_var("WASM_BINDGEN_USE_JS_SYS", "1");
        std::env::set_var("RUSTFLAGS", "--cfg wasm_bindgen_use_js_sys");
        let ambient_enabled = expand_async(&ExpansionBuilder::new(disabled.clone()));
        assert!(ExpansionContext::from_cargo_env()
            .unwrap()
            .use_js_sys_futures());

        std::env::remove_var("WASM_BINDGEN_USE_JS_SYS");
        std::env::set_var("RUSTFLAGS", "--cfg unrelated_ambient_cfg");
        let ambient_disabled = expand_async(&ExpansionBuilder::new(disabled));
        assert_eq!(
            ExpansionContext::from_cargo_env()
                .unwrap()
                .use_js_sys_futures(),
            cfg!(wasm_bindgen_use_js_sys)
        );

        assert_eq!(ambient_enabled, disabled_output);
        assert_eq!(ambient_disabled, disabled_output);
    }
}

pub fn expand_struct_marker(item: TokenStream) -> Result<TokenStream, Diagnostic> {
    parser::reset_attrs_used();

    let mut s: syn::ItemStruct = syn::parse2(item)?;

    let mut program = ast::Program::new(ExpansionContext::from_cargo_env()?);
    program.structs.push((&mut s).convert(&program)?);

    let mut tokens = proc_macro2::TokenStream::new();
    program.try_to_tokens(&mut tokens)?;

    parser::check_unused_attrs(&mut tokens);

    Ok(tokens)
}
