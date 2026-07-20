//! Proc macros for `cpython-api`: `#[pyfunction]` and `#[pymethods]`.
//!
//! These generate the `extern "C"` trampolines and argument-parsing glue —
//! the mechanical, error-prone part of binding Rust functions to Python.
//! Module and class *definitions* stay explicit (`export_module!`,
//! `ClassDef`); see RUST_API.md.
//!
//! Parameter classification is positional, verified by the type system
//! rather than by name matching (a proc macro sees only tokens — it cannot
//! resolve types or trait impls, so anything name-based would be a
//! heuristic):
//!
//! - after an optional `&self`, the **first** parameter is the thread-state
//!   token (`ts: &ThreadState<'py>`);
//! - the **second** is the module state (`state: &T` where `T: ModuleState`)
//!   — always present, mirroring how C module functions always receive the
//!   module object; stateless functions name it `_state`;
//! - the remaining parameters are Python-visible arguments extracted with
//!   `FromPyObject`.
//!
//! The macro only checks the *shape* (shared references in the right
//! positions); the generated code pins the real types, so a wrong type shows
//! up as an ordinary compile error at the user signature.
//!
//! ```ignore
//! #[pyfunction(signature = (data, value = 1, /))]
//! fn adler32(ts: &ThreadState<'_>, _state: &ZlibState,
//!            data: PyBuffer<'_>, value: u32) -> PyResult<u32> { .. }
//! // => mod adler32 { pub const DEF: PyMethodDef; ... }
//! ```
//!
//! `#[pymethods]` on an inherent impl block processes `#[pyfunction]`,
//! `#[getter]` and `#[new]` items and emits `{TYPE}_METHODS`,
//! `{TYPE}_GETSETS` and (if `#[new]` is present) `{TYPE}_TP_NEW` statics for
//! use with `ClassDef`. Methods are uniformly compiled as `METH_METHOD`
//! (PyCMethod), so the defining class — and through it the module state — is
//! always available. `#[getter]`s stay minimal (`&self, ts`), as does
//! `#[new]` (`ts, args...`).
//!
//! Note the inner attributes never expand on their own inside `#[pymethods]`:
//! attribute macros expand outermost-first, so the impl-block macro consumes
//! the markers before rustc would try to expand them.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Error, Expr, FnArg, Ident, ImplItem, ItemFn, ItemImpl, LitByteStr, LitStr, Pat, Result, Token,
    Type,
};

// --- signature attribute parsing -------------------------------------------

enum SigEntry {
    Param { name: Ident, default: Option<Expr> },
    PosOnlyMarker,
}

impl Parse for SigEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        if input.peek(Token![/]) {
            input.parse::<Token![/]>()?;
            return Ok(SigEntry::PosOnlyMarker);
        }
        let name: Ident = input.parse()?;
        let default = if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            Some(input.parse::<Expr>()?)
        } else {
            None
        };
        Ok(SigEntry::Param { name, default })
    }
}

/// The `signature = (...)` attribute payload.
struct PyFunctionAttr {
    entries: Option<Vec<SigEntry>>,
}

impl Parse for PyFunctionAttr {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        if input.is_empty() {
            return Ok(PyFunctionAttr { entries: None });
        }
        let key: Ident = input.parse()?;
        if key != "signature" {
            return Err(Error::new(key.span(), "expected `signature = (...)`"));
        }
        input.parse::<Token![=]>()?;
        let content;
        syn::parenthesized!(content in input);
        let entries: Punctuated<SigEntry, Token![,]> =
            content.parse_terminated(SigEntry::parse, Token![,])?;
        Ok(PyFunctionAttr {
            entries: Some(entries.into_iter().collect()),
        })
    }
}

// --- function model --------------------------------------------------------

struct PyParam {
    name: Ident,
    default: Option<Expr>,
}

/// Which special leading parameters the function shape carries.
#[derive(PartialEq, Clone, Copy)]
enum FnShape {
    /// `([&self,] ts, state, python-args...)`
    Regular,
    /// `(&self, ts)`
    Getter,
    /// `(ts, python-args...)`
    New,
}

struct FnModel {
    rust_name: Ident,
    has_receiver: bool,
    /// The token was declared `&mut ThreadState` (needed for `detach`).
    ts_mut: bool,
    /// Type behind the state reference (`Some` iff shape is `Regular`).
    state_ty: Option<Type>,
    params: Vec<PyParam>,
    pos_only: usize,
}

impl FnModel {
    /// `let [mut] __ts = ...` binding and the `&[mut] __ts` expression passed
    /// to the user function.
    fn ts_tokens(&self) -> (TokenStream2, TokenStream2) {
        if self.ts_mut {
            (quote!(mut __ts), quote!(&mut __ts))
        } else {
            (quote!(__ts), quote!(&__ts))
        }
    }
}

/// Positional slot for a special leading parameter: require a shared
/// reference and return its target type. The *actual* type is pinned by the
/// generated code, so this only guards the shape for a clear early error.
fn shared_ref_target(ty: &Type, what: &str) -> Result<Type> {
    let Type::Reference(r) = ty else {
        return Err(Error::new(
            ty.span(),
            format!("this parameter must be {what}"),
        ));
    };
    if r.mutability.is_some() {
        return Err(Error::new(
            ty.span(),
            format!("this parameter must be a shared reference: {what}"),
        ));
    }
    Ok(r.elem.as_ref().clone())
}

/// Analyze a function signature and reconcile it with the `signature = (...)`
/// attribute.
fn analyze_fn(sig: &syn::Signature, attr: &PyFunctionAttr, shape: FnShape) -> Result<FnModel> {
    let mut has_receiver = false;
    let mut saw_ts = false;
    let mut ts_mut = false;
    let mut state_ty: Option<Type> = None;
    let mut py_names: Vec<Ident> = Vec::new();

    for input in sig.inputs.iter() {
        match input {
            FnArg::Receiver(recv) => {
                if recv.mutability.is_some() || recv.reference.is_none() {
                    return Err(Error::new(
                        recv.span(),
                        "methods must take `&self`; use interior mutability (e.g. Mutex) for state",
                    ));
                }
                has_receiver = true;
            }
            FnArg::Typed(pat_ty) => {
                if !saw_ts {
                    // `&ThreadState` or `&mut ThreadState` (the latter allows
                    // `ts.detach(..)` in the body).
                    let Type::Reference(r) = &*pat_ty.ty else {
                        return Err(Error::new(
                            pat_ty.ty.span(),
                            "the first parameter must be the thread-state token \
                             (`ts: &ThreadState` or `ts: &mut ThreadState`)",
                        ));
                    };
                    ts_mut = r.mutability.is_some();
                    saw_ts = true;
                    continue;
                }
                if shape == FnShape::Regular && state_ty.is_none() {
                    state_ty = Some(shared_ref_target(
                        &pat_ty.ty,
                        "the module state (`state: &YourModuleState`)",
                    )?);
                    continue;
                }
                let Pat::Ident(pi) = pat_ty.pat.as_ref() else {
                    return Err(Error::new(
                        pat_ty.pat.span(),
                        "parameter patterns are not supported; use a plain name",
                    ));
                };
                py_names.push(pi.ident.clone());
            }
        }
    }
    if !saw_ts {
        return Err(Error::new(
            sig.span(),
            "a `ts: &ThreadState` parameter is required (first after `&self`)",
        ));
    }
    if shape == FnShape::Regular && state_ty.is_none() {
        return Err(Error::new(
            sig.span(),
            "a module-state parameter is required after `ts` (`state: &YourModuleState`; \
             name it `_state` if unused)",
        ));
    }
    if shape == FnShape::Getter && !py_names.is_empty() {
        return Err(Error::new(
            sig.span(),
            "#[getter] methods must be `fn name(&self, ts: &ThreadState) -> T`",
        ));
    }

    // Reconcile with the signature attribute.
    let (params, pos_only) = match &attr.entries {
        None => (
            py_names
                .into_iter()
                .map(|name| PyParam {
                    name,
                    default: None,
                })
                .collect::<Vec<_>>(),
            0,
        ),
        Some(entries) => {
            let mut params = Vec::new();
            let mut pos_only = None;
            for entry in entries {
                match entry {
                    SigEntry::PosOnlyMarker => {
                        if pos_only.is_some() {
                            return Err(Error::new(sig.span(), "duplicate `/` in signature"));
                        }
                        pos_only = Some(params.len());
                    }
                    SigEntry::Param { name, default } => params.push(PyParam {
                        name: name.clone(),
                        default: default.clone(),
                    }),
                }
            }
            let sig_names: Vec<String> = params.iter().map(|p| p.name.to_string()).collect();
            let fn_names: Vec<String> = py_names.iter().map(|i| i.to_string()).collect();
            if sig_names != fn_names {
                return Err(Error::new(
                    sig.span(),
                    format!(
                        "signature parameters ({}) do not match function parameters ({})",
                        sig_names.join(", "),
                        fn_names.join(", ")
                    ),
                ));
            }
            (params, pos_only.unwrap_or(0))
        }
    };

    Ok(FnModel {
        rust_name: sig.ident.clone(),
        has_receiver,
        ts_mut,
        state_ty,
        params,
        pos_only,
    })
}

// --- code generation -------------------------------------------------------

fn cstr_lit(s: &str, span: proc_macro2::Span) -> LitByteStr {
    LitByteStr::new(format!("{s}\0").as_bytes(), span)
}

fn gen_spec(model: &FnModel) -> TokenStream2 {
    let fn_name = LitStr::new(&model.rust_name.to_string(), model.rust_name.span());
    let pos_only = model.pos_only;
    let params = model.params.iter().map(|p| {
        let name = cstr_lit(&p.name.to_string(), p.name.span());
        let required = p.default.is_none();
        quote! {
            ::cpython_api::args::Param {
                name: ::cpython_api::const_cstr(#name),
                required: #required,
            }
        }
    });
    quote! {
        pub const SPEC: ::cpython_api::args::ParamSpec = ::cpython_api::args::ParamSpec {
            fn_name: #fn_name,
            params: &[#(#params),*],
            pos_only: #pos_only,
        };
    }
}

/// Generate the `let vN = ...` extraction statements (shared by all
/// trampoline kinds).
fn gen_extractions(model: &FnModel) -> (Vec<TokenStream2>, Vec<Ident>) {
    let mut stmts = Vec::new();
    let mut vars = Vec::new();
    for (i, param) in model.params.iter().enumerate() {
        let var = format_ident!("__arg{i}");
        let stmt = match &param.default {
            Some(default) => quote! {
                let #var = match ::cpython_api::args::arg_bound(&__slots, #i) {
                    Some(obj) => ::cpython_api::FromPyObject::extract(&__ts, obj)?,
                    None => #default,
                };
            },
            None => quote! {
                let #var = match ::cpython_api::args::arg_bound(&__slots, #i) {
                    Some(obj) => ::cpython_api::FromPyObject::extract(&__ts, obj)?,
                    None => unreachable!("required argument enforced by parse"),
                };
            },
        };
        stmts.push(stmt);
        vars.push(var);
    }
    (stmts, vars)
}

enum TrampolineKind<'a> {
    /// Free function; `self` is the module object (state source).
    Function,
    /// Instance method of `self_ty`; METH_METHOD, state via defining class.
    Method { self_ty: &'a Type },
}

/// Generate the trampoline plus `DEF` for a function or method.
fn gen_trampoline_and_def(model: &FnModel, kind: TrampolineKind<'_>) -> Result<TokenStream2> {
    let n = model.params.len();
    let (extractions, vars) = gen_extractions(model);
    let name = &model.rust_name;
    let ml_name = cstr_lit(&model.rust_name.to_string(), model.rust_name.span());
    let state_ty = model
        .state_ty
        .as_ref()
        .expect("Regular shape always has a state type");

    let (ts_binding, ts_arg) = model.ts_tokens();
    let (state_setup, call) = match &kind {
        TrampolineKind::Function => (
            quote! {
                let __state: &#state_ty = unsafe {
                    ::cpython_api::module::state_from_module_ptr::<#state_ty>(&__ts, __slf)?
                };
            },
            quote! { super::#name(#ts_arg, __state, #(#vars),*) },
        ),
        TrampolineKind::Method { self_ty } => (
            quote! {
                let __this: &#self_ty =
                    unsafe { ::cpython_api::class::payload_ref::<#self_ty>(__slf) };
                let __state: &#state_ty = unsafe {
                    ::cpython_api::class::state_from_defining_class::<#state_ty>(&__ts, __cls)?
                };
            },
            quote! { super::#self_ty::#name(__this, #ts_arg, __state, #(#vars),*) },
        ),
    };

    let body = quote! {
        let #ts_binding = unsafe { ::cpython_api::ThreadState::current_unchecked() };
        let __result: ::cpython_api::PyResult<*mut ::cpython_api::ffi::PyObject> = (|| {
            #state_setup
            let __slots = unsafe {
                ::cpython_api::args::parse_fastcall::<#n>(
                    &__ts, &SPEC, __args as *const _, __nargs, __kwnames,
                )
            }?;
            #(#extractions)*
            let __ret = #call;
            ::cpython_api::conversion::IntoPyCallbackOutput::convert(__ret, &__ts)
        })();
        match __result {
            Ok(ptr) => ptr,
            Err(_raised) => ::std::ptr::null_mut(),
        }
    };

    let (trampoline, def) = match &kind {
        TrampolineKind::Method { .. } => (
            quote! {
                pub(crate) unsafe extern "C" fn trampoline(
                    __slf: *mut ::cpython_api::ffi::PyObject,
                    __cls: *mut ::cpython_api::ffi::PyTypeObject,
                    __args: *mut *mut ::cpython_api::ffi::PyObject,
                    __nargs: ::cpython_api::ffi::Py_ssize_t,
                    __kwnames: *mut ::cpython_api::ffi::PyObject,
                ) -> *mut ::cpython_api::ffi::PyObject {
                    #body
                }
            },
            quote! {
                pub const DEF: ::cpython_api::ffi::PyMethodDef = ::cpython_api::ffi::PyMethodDef {
                    ml_name: ::cpython_api::const_cstr(#ml_name).as_ptr().cast_mut(),
                    ml_meth: ::cpython_api::ffi::PyMethodDefFuncPointer {
                        PyCMethod: trampoline,
                    },
                    ml_flags: ::cpython_api::ffi::METH_FASTCALL
                        | ::cpython_api::ffi::METH_KEYWORDS
                        | ::cpython_api::ffi::METH_METHOD,
                    ml_doc: ::std::ptr::null_mut(),
                };
            },
        ),
        TrampolineKind::Function => (
            quote! {
                pub(crate) unsafe extern "C" fn trampoline(
                    __slf: *mut ::cpython_api::ffi::PyObject,
                    __args: *mut *mut ::cpython_api::ffi::PyObject,
                    __nargs: ::cpython_api::ffi::Py_ssize_t,
                    __kwnames: *mut ::cpython_api::ffi::PyObject,
                ) -> *mut ::cpython_api::ffi::PyObject {
                    #body
                }
            },
            quote! {
                pub const DEF: ::cpython_api::ffi::PyMethodDef = ::cpython_api::ffi::PyMethodDef {
                    ml_name: ::cpython_api::const_cstr(#ml_name).as_ptr().cast_mut(),
                    ml_meth: ::cpython_api::ffi::PyMethodDefFuncPointer {
                        PyCFunctionFastWithKeywords: trampoline,
                    },
                    ml_flags: ::cpython_api::ffi::METH_FASTCALL
                        | ::cpython_api::ffi::METH_KEYWORDS,
                    ml_doc: ::std::ptr::null_mut(),
                };
            },
        ),
    };

    let spec = gen_spec(model);
    Ok(quote! {
        #spec
        #trampoline
        #def
    })
}

// --- #[pyfunction] on free functions ---------------------------------------

#[proc_macro_attribute]
pub fn pyfunction(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = syn::parse_macro_input!(attr as PyFunctionAttr);
    let func = syn::parse_macro_input!(item as ItemFn);

    let model = match analyze_fn(&func.sig, &attr, FnShape::Regular) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };
    if model.has_receiver {
        return Error::new(
            func.sig.span(),
            "#[pyfunction] on methods must be used inside #[pymethods]",
        )
        .to_compile_error()
        .into();
    }

    let generated = match gen_trampoline_and_def(&model, TrampolineKind::Function) {
        Ok(g) => g,
        Err(e) => return e.to_compile_error().into(),
    };

    let vis = &func.vis;
    let mod_name = &model.rust_name;
    let out = quote! {
        #func

        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        #vis mod #mod_name {
            use super::*;
            #generated
        }
    };
    out.into()
}

// --- #[pymethods] on impl blocks -------------------------------------------

enum MethodKind {
    Regular,
    Getter,
    New,
}

#[proc_macro_attribute]
pub fn pymethods(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut block = syn::parse_macro_input!(item as ItemImpl);
    match expand_pymethods(&mut block) {
        Ok(extra) => quote! { #block #extra }.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_pymethods(block: &mut ItemImpl) -> Result<TokenStream2> {
    if block.trait_.is_some() {
        return Err(Error::new(
            block.span(),
            "#[pymethods] only supports inherent impl blocks",
        ));
    }
    let self_ty = block.self_ty.as_ref().clone();
    let Type::Path(self_path) = &self_ty else {
        return Err(Error::new(block.self_ty.span(), "unsupported self type"));
    };
    let type_ident = &self_path.path.segments.last().unwrap().ident;
    let prefix = type_ident.to_string().to_uppercase();

    let mut method_defs: Vec<TokenStream2> = Vec::new();
    let mut getset_entries: Vec<TokenStream2> = Vec::new();
    let mut generated_mods: Vec<TokenStream2> = Vec::new();
    let mut tp_new: Option<TokenStream2> = None;

    for item in block.items.iter_mut() {
        let ImplItem::Fn(method) = item else { continue };

        // Recognize and strip our marker attributes.
        let mut kind = MethodKind::Regular;
        let mut fn_attr = PyFunctionAttr { entries: None };
        let mut keep = Vec::new();
        let mut is_py = false;
        for attr in method.attrs.drain(..) {
            if attr.path().is_ident("pyfunction") {
                is_py = true;
                fn_attr = match &attr.meta {
                    syn::Meta::Path(_) => PyFunctionAttr { entries: None },
                    _ => attr.parse_args::<PyFunctionAttr>()?,
                };
            } else if attr.path().is_ident("getter") {
                is_py = true;
                kind = MethodKind::Getter;
            } else if attr.path().is_ident("new") {
                is_py = true;
                kind = MethodKind::New;
            } else {
                keep.push(attr);
            }
        }
        method.attrs = keep;
        if !is_py {
            continue;
        }

        let shape = match kind {
            MethodKind::Regular => FnShape::Regular,
            MethodKind::Getter => FnShape::Getter,
            MethodKind::New => FnShape::New,
        };
        let model = analyze_fn(&method.sig, &fn_attr, shape)?;
        let rust_name = model.rust_name.clone();
        let mod_name = format_ident!("__cpy_{}_{}", type_ident, rust_name);

        match kind {
            MethodKind::Regular => {
                if !model.has_receiver {
                    return Err(Error::new(
                        method.sig.span(),
                        "#[pyfunction] methods must take &self (use #[new] for constructors)",
                    ));
                }
                let generated =
                    gen_trampoline_and_def(&model, TrampolineKind::Method { self_ty: &self_ty })?;
                generated_mods.push(quote! {
                    #[doc(hidden)]
                    #[allow(non_snake_case, non_upper_case_globals)]
                    mod #mod_name {
                        use super::*;
                        #generated
                    }
                });
                method_defs.push(quote!(#mod_name::DEF));
            }
            MethodKind::Getter => {
                if !model.has_receiver {
                    return Err(Error::new(
                        method.sig.span(),
                        "#[getter] methods must take &self",
                    ));
                }
                let name_lit = cstr_lit(&rust_name.to_string(), rust_name.span());
                let (ts_binding, ts_arg) = model.ts_tokens();
                generated_mods.push(quote! {
                    #[doc(hidden)]
                    #[allow(non_snake_case)]
                    mod #mod_name {
                        use super::*;
                        pub(crate) unsafe extern "C" fn getter(
                            __slf: *mut ::cpython_api::ffi::PyObject,
                            _closure: *mut ::std::os::raw::c_void,
                        ) -> *mut ::cpython_api::ffi::PyObject {
                            let #ts_binding = unsafe { ::cpython_api::ThreadState::current_unchecked() };
                            let __result: ::cpython_api::PyResult<*mut ::cpython_api::ffi::PyObject> = (|| {
                                let __this: &#self_ty =
                                    unsafe { ::cpython_api::class::payload_ref::<#self_ty>(__slf) };
                                let __ret = super::#self_ty::#rust_name(__this, #ts_arg);
                                ::cpython_api::conversion::IntoPyCallbackOutput::convert(__ret, &__ts)
                            })();
                            match __result {
                                Ok(ptr) => ptr,
                                Err(_raised) => ::std::ptr::null_mut(),
                            }
                        }
                    }
                });
                getset_entries.push(quote! {
                    ::cpython_api::ffi::PyGetSetDef {
                        name: ::cpython_api::const_cstr(#name_lit).as_ptr(),
                        get: Some(#mod_name::getter),
                        set: None,
                        doc: ::std::ptr::null(),
                        closure: ::std::ptr::null_mut(),
                    }
                });
            }
            MethodKind::New => {
                if model.has_receiver {
                    return Err(Error::new(
                        method.sig.span(),
                        "#[new] must not take a receiver; it returns PyResult<Self>",
                    ));
                }
                if tp_new.is_some() {
                    return Err(Error::new(method.sig.span(), "duplicate #[new]"));
                }
                let n = model.params.len();
                let (extractions, vars) = gen_extractions(&model);
                let spec = gen_spec(&model);
                let (ts_binding, ts_arg) = model.ts_tokens();
                generated_mods.push(quote! {
                    #[doc(hidden)]
                    #[allow(non_snake_case)]
                    mod #mod_name {
                        use super::*;
                        #spec
                        pub(crate) unsafe extern "C" fn tp_new(
                            __subtype: *mut ::cpython_api::ffi::PyTypeObject,
                            __args: *mut ::cpython_api::ffi::PyObject,
                            __kwargs: *mut ::cpython_api::ffi::PyObject,
                        ) -> *mut ::cpython_api::ffi::PyObject {
                            let #ts_binding = unsafe { ::cpython_api::ThreadState::current_unchecked() };
                            let __result: ::cpython_api::PyResult<*mut ::cpython_api::ffi::PyObject> = (|| {
                                let __slots = unsafe {
                                    ::cpython_api::args::parse_tuple_dict::<#n>(
                                        &__ts, &SPEC, __args, __kwargs,
                                    )
                                }?;
                                #(#extractions)*
                                let __payload: #self_ty =
                                    super::#self_ty::#rust_name(#ts_arg, #(#vars),*)?;
                                unsafe {
                                    ::cpython_api::class::alloc_with::<#self_ty>(
                                        &__ts, __subtype, __payload,
                                    )
                                }
                            })();
                            match __result {
                                Ok(ptr) => ptr,
                                Err(_raised) => ::std::ptr::null_mut(),
                            }
                        }
                    }
                });
                tp_new = Some(quote!(#mod_name::tp_new));
            }
        }
    }

    let methods_ident = format_ident!("{prefix}_METHODS");
    let getsets_ident = format_ident!("{prefix}_GETSETS");
    let n_methods = method_defs.len() + 1;
    let n_getsets = getset_entries.len() + 1;

    let tp_new_item = tp_new.map(|path| {
        let ident = format_ident!("{prefix}_TP_NEW");
        quote! {
            pub(crate) const #ident: ::cpython_api::class::NewFunc = #path;
        }
    });

    Ok(quote! {
        #(#generated_mods)*

        pub(crate) static #methods_ident: ::cpython_api::module::MethodDefs<#n_methods> =
            ::cpython_api::module::MethodDefs([
                #(#method_defs,)*
                ::cpython_api::ffi::PyMethodDef::zeroed(),
            ]);

        pub(crate) static #getsets_ident: ::cpython_api::module::GetSetDefs<#n_getsets> =
            ::cpython_api::module::GetSetDefs([
                #(#getset_entries,)*
                ::cpython_api::ffi::PyGetSetDef {
                    name: ::std::ptr::null(),
                    get: None,
                    set: None,
                    doc: ::std::ptr::null(),
                    closure: ::std::ptr::null_mut(),
                },
            ]);

        #tp_new_item
    })
}
