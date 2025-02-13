//! This crate removes some boilerplate for structs that simply delegate
//! some of their methods to one or more of their fields.
//!
//! It gives you the `delegate!` macro, which delegates method calls to selected expressions (usually inner fields).
//!
//! ## Features:
//! - Delegate to a method with a different name
//! ```rust
//! use delegate::delegate;
//!
//! struct Stack { inner: Vec<u32> }
//! impl Stack {
//!     delegate! {
//!         to self.inner {
//!             #[call(push)]
//!             pub fn add(&mut self, value: u32);
//!         }
//!     }
//! }
//! ```
//! - Use an arbitrary inner field expression
//! ```rust
//! use delegate::delegate;
//!
//! use std::rc::Rc;
//! use std::cell::RefCell;
//! use std::ops::Deref;
//!
//! struct Wrapper { inner: Rc<RefCell<Vec<u32>>> }
//! impl Wrapper {
//!     delegate! {
//!         to self.inner.deref().borrow_mut() {
//!             pub fn push(&mut self, val: u32);
//!         }
//!     }
//! }
//! ```
//! - Change the return type of the delegated method using a `From` impl or omit it altogether
//! ```rust
//! use delegate::delegate;
//!
//! struct Inner;
//! impl Inner {
//!     pub fn method(&self, num: u32) -> u32 { num }
//! }
//! struct Wrapper { inner: Inner }
//! impl Wrapper {
//!     delegate! {
//!         to self.inner {
//!             // calls method, converts result to u64
//!             #[into]
//!             pub fn method(&self, num: u32) -> u64;
//!
//!             // calls method, returns ()
//!             #[call(method)]
//!             pub fn method_noreturn(&self, num: u32);
//!         }
//!     }
//! }
//! ```
//! - Call `await` on async functions
//! ```rust
//! use delegate::delegate;
//!
//! struct Inner;
//! impl Inner {
//!     pub async fn method(&self, num: u32) -> u32 { num }
//! }
//! struct Wrapper { inner: Inner }
//! impl Wrapper {
//!     delegate! {
//!         to self.inner {
//!             // calls method(num).await, returns impl Future<Output = u32>
//!             pub async fn method(&self, num: u32) -> u32;
//!
//!             // calls method(num).await.into(), returns impl Future<Output = u64>
//!             #[into]
//!             #[call(method)]
//!             pub async fn method_into(&self, num: u32) -> u64;
//!         }
//!     }
//! }
//! ```
//! - Delegate to multiple fields
//! ```rust
//! use delegate::delegate;
//!
//! struct MultiStack {
//!     left: Vec<u32>,
//!     right: Vec<u32>,
//! }
//! impl MultiStack {
//!     delegate! {
//!         to self.left {
//!             // Push an item to the top of the left stack
//!             #[call(push)]
//!             pub fn push_left(&mut self, value: u32);
//!         }
//!         to self.right {
//!             // Push an item to the top of the right stack
//!             #[call(push)]
//!             pub fn push_right(&mut self, value: u32);
//!         }
//!     }
//! }
//! ```
//! - Inserts `#[inline(always)]` automatically (unless you specify `#[inline]` manually on the method)
//! - Specify expressions in the signature that will be used as delegated arguments
//! ```rust
//! use delegate::delegate;
//! struct Inner;
//! impl Inner {
//!     pub fn polynomial(&self, a: i32, x: i32, b: i32, y: i32, c: i32) -> i32 {
//!         a + x * x + b * y + c
//!     }
//! }
//! struct Wrapper { inner: Inner, a: i32, b: i32, c: i32 }
//! impl Wrapper {
//!     delegate! {
//!         to self.inner {
//!             // Calls `polynomial` on `inner` with `self.a`, `self.b` and
//!             // `self.c` passed as arguments `a`, `b`, and `c`, effectively
//!             // calling `polynomial(self.a, x, self.b, y, self.c)`.
//!             pub fn polynomial(&self, [ self.a ], x: i32, [ self.b ], y: i32, [ self.c ]) -> i32 ;
//!             // Calls `polynomial` on `inner` with `0`s passed for arguments
//!             // `a` and `x`, and `self.b` and `self.c` for `b` and `c`,
//!             // effectively calling `polynomial(0, 0, self.b, y, self.c)`.
//!             #[call(polynomial)]
//!             pub fn linear(&self, [ 0 ], [ 0 ], [ self.b ], y: i32, [ self.c ]) -> i32 ;
//!         }
//!     }
//! }

extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;

use quote::quote;
use std::collections::HashMap;
use syn::parse::ParseStream;
use syn::spanned::Spanned;
use syn::Error;

mod kw {
    syn::custom_keyword!(to);
    syn::custom_keyword!(target);
}

#[derive(Clone)]
enum DelegatedInput {
    Input(syn::FnArg),
    Argument(syn::Expr),
}

impl syn::parse::Parse for DelegatedInput {
    fn parse(input: syn::parse::ParseStream) -> Result<Self, Error> {
        let lookahead = input.lookahead1();
        if lookahead.peek(syn::token::Bracket) {
            let content;
            let _bracket_token = syn::bracketed!(content in input);
            let expression: syn::Expr = content.parse()?;
            Ok(Self::Argument(expression))
        } else {
            let input: syn::FnArg = input.parse()?;
            Ok(Self::Input(input))
        }
    }
}

struct DelegatedMethod {
    method: syn::TraitItemMethod,
    attributes: Vec<syn::Attribute>,
    visibility: syn::Visibility,
    arguments: syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
}

// Given an input parameter from a function signature, create a functiona
// argument used to call the delegate function: omit receiver, extract an
// identifier from a typed input parameter (and wrap it in an `Expr`).
fn parse_input_into_argument_expression(
    function_name: &syn::Ident,
    input: &syn::FnArg,
) -> Option<syn::Expr> {
    match input {
        // Parse inputs of the form `x: T` to retrieve their identifiers.
        syn::FnArg::Typed(typed) => {
            match &*typed.pat {
                // This should not happen, I think. If it does,
                // it will be ignored as if it were the
                // receiver.
                syn::Pat::Ident(ident) if ident.ident == "self" => None,
                // Expression in the form`x: T`. Extract the
                // identifier, wrap it in Expr for type-
                // compatibility with bracketed expressions, and append it
                // to the argument list.
                syn::Pat::Ident(ident) => {
                    let path_segment = syn::PathSegment {
                        ident: ident.ident.clone(),
                        arguments: syn::PathArguments::None,
                    };
                    let mut segments = syn::punctuated::Punctuated::new();
                    segments.push(path_segment);
                    let path = syn::Path {
                        leading_colon: None,
                        segments,
                    };
                    let ident_as_expr = syn::Expr::from(syn::ExprPath {
                        attrs: Vec::new(),
                        qself: None,
                        path,
                    });
                    Some(ident_as_expr)
                }
                // Other more complex argument expressions are not covered.
                _ => panic!(
                    "You have to use simple identifiers for delegated method parameters ({})",
                    function_name // The signature is not constructed yet. We make due.
                ),
            }
        }
        // Skip any `self`/`&self`/`&mut self` argument, since
        // it does not appear in the argument list and it's
        // already added to the parameter list.
        syn::FnArg::Receiver(_receiver) => None,
    }
}

impl syn::parse::Parse for DelegatedMethod {
    fn parse(input: ParseStream) -> Result<Self, Error> {
        let attributes = input.call(syn::Attribute::parse_outer)?;
        let visibility = input.call(syn::Visibility::parse)?;

        // Unchanged from Parse from TraitItemMethod
        let constness: Option<syn::Token![const]> = input.parse()?;
        let asyncness: Option<syn::Token![async]> = input.parse()?;
        let unsafety: Option<syn::Token![unsafe]> = input.parse()?;
        let abi: Option<syn::Abi> = input.parse()?;
        let fn_token: syn::Token![fn] = input.parse()?;
        let ident: syn::Ident = input.parse()?;
        let generics: syn::Generics = input.parse()?;

        let content;
        let paren_token = syn::parenthesized!(content in input);

        // Parse inputs (method parameters) and arguments. The parameters
        // constitute the parameter list of the signature of the delegating
        // method so it must include all inputs, except bracketed expressions.
        // The argument list constitutes the list of arguments used to call the
        // delegated function. It must include all inputs, excluding the
        // receiver (self-type) input. The arguments must all be parsed to
        // retrieve the expressions inside of the brackets as well as variable
        // identifiers of ordinary inputs. The arguments must preserve the order
        // of the inputs.
        let delegated_inputs =
            content.parse_terminated::<DelegatedInput, syn::Token![,]>(DelegatedInput::parse)?;
        let mut inputs: syn::punctuated::Punctuated<syn::FnArg, syn::Token![,]> =
            syn::punctuated::Punctuated::new();
        let mut arguments: syn::punctuated::Punctuated<syn::Expr, syn::Token![,]> =
            syn::punctuated::Punctuated::new();

        // First, combine the cases for pairs with cases for end, to remove
        // redundancy below.
        delegated_inputs
            .into_pairs()
            .map(|punctuated_pair| match punctuated_pair {
                syn::punctuated::Pair::Punctuated(item, comma) => (item, Some(comma)),
                syn::punctuated::Pair::End(item) => (item, None),
            })
            .for_each(|pair| match pair {
                // This input is a bracketed argument (eg. `[ self.x ]`). It
                // is omitted in the signature of the delegator, but the
                // expression inside the brackets is used in the body of the
                // delegator, as an arugnment to the delegated function (eg.
                // `self.x`). The argument needs to be generated in the
                // appropriate position with respect other arguments and non-
                // argument inputs. As long as inputs are added to the
                // `arguments` vector in order of occurance, this is trivial.
                (DelegatedInput::Argument(argument), maybe_comma) => {
                    arguments.push_value(argument);
                    if let Some(comma) = maybe_comma {
                        arguments.push_punct(comma)
                    }
                }
                // The input is a standard function parameter with a name and
                // a type (eg. `x: T`). This input needs to be reflected in
                // the delegator signature as is (eg. `x: T`). The identifier
                // also needs to be included in the argument list in part
                // (eg. `x`). The argument list needs to preserve the order of
                // the inputs with relation to arguments (see above), so the
                // parsing is best done here (previously it was done at
                // generation).
                (DelegatedInput::Input(input), maybe_comma) => {
                    inputs.push_value(input.clone());
                    if let Some(comma) = maybe_comma {
                        inputs.push_punct(comma);
                    }
                    let maybe_argument = parse_input_into_argument_expression(&ident, &input);
                    if let Some(argument) = maybe_argument {
                        arguments.push(argument);
                        if let Some(comma) = maybe_comma {
                            arguments.push_punct(comma);
                        }
                    }
                }
            });

        // Unchanged from Parse from TraitItemMethod
        let output: syn::ReturnType = input.parse()?;
        let where_clause: Option<syn::WhereClause> = input.parse()?;

        // This needs to be generated manually, because inputs need to be
        // separated into actual inputs that go in the signature (the
        // parameters) and the additional expressions in square brackets which
        // go into the arguments vector (artguments of the call on the method
        // on the inner object).
        let signature = syn::Signature {
            constness,
            asyncness,
            unsafety,
            abi,
            fn_token,
            ident,
            paren_token,
            inputs,
            output,
            variadic: None,
            generics: syn::Generics {
                where_clause,
                ..generics
            },
        };

        // Check if the input contains a semicolon or a brace. If it contains
        // a semicolon, we parse it (to retain token location information) and
        // continue. However, if it contains a brace, this indicates that
        // there is a default definition of the method. This is not supported,
        // so in that case we error out.
        let lookahead = input.lookahead1();
        let semi_token: Option<syn::Token![;]>;
        if lookahead.peek(syn::Token![;]) {
            semi_token = Some(input.parse()?);
        } else {
            panic!(
                "Do not include implementation of delegated functions ({})",
                signature.ident
            );
        }

        // This needs to be populated from scratch because of the signature above.
        let method = syn::TraitItemMethod {
            // All attributes are attached to `DelegatedMethod`, since they
            // presumably pertain to the process of delegation, not the
            // signature of the delegator.
            attrs: Vec::new(),
            sig: signature,
            default: None,
            semi_token,
        };

        Ok(DelegatedMethod {
            method,
            attributes,
            visibility,
            arguments,
        })
    }
}

struct DelegatedSegment {
    delegator: syn::Expr,
    methods: Vec<DelegatedMethod>,
}

impl syn::parse::Parse for DelegatedSegment {
    fn parse(input: ParseStream) -> Result<Self, Error> {
        if let Ok(keyword) = input.parse::<kw::target>() {
            return Err(Error::new(keyword.span(), "You are using the old `target` expression, which is deprecated. Please replace `target` with `to`."));
        } else {
            input.parse::<kw::to>()?;
        }

        syn::Expr::parse_without_eager_brace(input).and_then(|delegator| {
            let content;
            syn::braced!(content in input);

            let mut methods = vec![];
            while !content.is_empty() {
                methods.push(content.parse::<DelegatedMethod>().unwrap());
            }

            Ok(DelegatedSegment { delegator, methods })
        })
    }
}

struct DelegationBlock {
    segments: Vec<DelegatedSegment>,
}

impl syn::parse::Parse for DelegationBlock {
    fn parse(input: ParseStream) -> Result<Self, Error> {
        let mut segments = vec![];
        while !input.is_empty() {
            segments.push(input.parse()?);
        }

        Ok(DelegationBlock { segments })
    }
}

struct CallMethodAttribute {
    name: syn::Ident,
}

impl syn::parse::Parse for CallMethodAttribute {
    fn parse(input: ParseStream) -> Result<Self, Error> {
        let content;
        syn::parenthesized!(content in input);
        Ok(CallMethodAttribute {
            name: content.parse()?,
        })
    }
}

/// Iterates through the attributes of a method and filters special attributes.
/// call => sets the name of the target method to call
/// into => generates a `into()` call for the returned value
///
/// Returns tuple (blackbox attributes, name, into)
fn parse_attributes<'a>(
    attrs: &'a [syn::Attribute],
    method: &syn::TraitItemMethod,
) -> (Vec<&'a syn::Attribute>, Option<syn::Ident>, bool) {
    let mut name: Option<syn::Ident> = None;
    let mut into: Option<bool> = None;
    let mut map: HashMap<&str, Box<dyn FnMut(TokenStream2)>> = Default::default();
    map.insert(
        "call",
        Box::new(|stream| {
            let target = syn::parse2::<CallMethodAttribute>(stream).unwrap();
            if name.is_some() {
                panic!(
                    "Multiple call attributes specified for {}",
                    method.sig.ident
                )
            }
            name = Some(target.name);
        }),
    );
    map.insert(
        "target_method",
        Box::new(|_| {
            panic!("You are using the old `target_method` attribute, which is deprecated. Please replace `target_method` with `call`.");
        }),
    );
    map.insert(
        "into",
        Box::new(|_| {
            if into.is_some() {
                panic!(
                    "Multiple into attributes specified for {}",
                    method.sig.ident
                )
            }
            into = Some(true);
        }),
    );
    let attrs: Vec<&syn::Attribute> = attrs
        .iter()
        .filter(|attr| {
            if let syn::AttrStyle::Outer = attr.style {
                for (ident, callback) in map.iter_mut() {
                    if attr.path.is_ident(ident) {
                        callback(attr.tokens.clone());
                        return false;
                    }
                }
            }

            true
        })
        .collect();

    drop(map);
    (attrs, name, into.unwrap_or(false))
}

/// Returns true if there are any `inline` attributes in the input.
fn has_inline_attribute(attrs: &[&syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if let syn::AttrStyle::Outer = attr.style {
            attr.path.is_ident("inline")
        } else {
            false
        }
    })
}

#[proc_macro]
pub fn delegate(tokens: TokenStream) -> TokenStream {
    let block: DelegationBlock = syn::parse_macro_input!(tokens);
    let sections = block.segments.iter().map(|delegator| {
        let delegator_attribute = &delegator.delegator;
        let functions = delegator.methods.iter().map(|method| {
            let input = &method.method;
            let signature = &input.sig;

            let (attrs, name, into) = parse_attributes(&method.attributes, input);

            if input.default.is_some() {
                panic!(
                    "Do not include implementation of delegated functions ({})",
                    signature.ident
                );
            }

            // Generate an argument vector from Punctuated list.
            let args: Vec<syn::Expr> = method.arguments.clone().into_iter().collect();

            let name = match &name {
                Some(n) => n,
                None => &input.sig.ident,
            };
            let inline = if has_inline_attribute(&attrs) {
                quote!()
            } else {
                quote! { #[inline(always)] }
            };
            let visibility = &method.visibility;

            let body = quote::quote! { #delegator_attribute.#name(#(#args),*) };
            let body = if signature.asyncness.is_some() {
                quote::quote! { #body.await }
            } else {
                body
            };

            let span = input.span();
            let body = match &signature.output {
                syn::ReturnType::Default => quote::quote! { #body; },
                syn::ReturnType::Type(_, ret_type) => {
                    if into {
                        quote::quote! { std::convert::Into::<#ret_type>::into(#body) }
                    } else {
                        body
                    }
                }
            };

            quote::quote_spanned! {span=>
                #(#attrs)*
                #inline
                #visibility #signature {
                    #body
                }
            }
        });

        quote! { #(#functions)* }
    });

    let result = quote! {
        #(#sections)*
    };
    result.into()
}
