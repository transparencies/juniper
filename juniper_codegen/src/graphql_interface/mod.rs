//! Code generation for [GraphQL interface][1].
//!
//! [1]: https://spec.graphql.org/June2018/#sec-Interfaces

pub mod attr;

use std::collections::{HashMap, HashSet};

use proc_macro2::{Span, TokenStream};
use quote::{quote, ToTokens, TokenStreamExt as _};
use syn::{
    parse::{Parse, ParseStream},
    parse_quote,
    spanned::Spanned as _,
    token,
};

use crate::util::{
    dup_attr_err, filter_attrs, get_doc_comment, span_container::SpanContainer, OptionExt as _,
    ParseBufferExt as _,
};

/*
/// Helper alias for the type of [`InterfaceMeta::external_downcasters`] field.
type InterfaceMetaDowncasters = HashMap<syn::Type, SpanContainer<syn::ExprPath>>;*/

/// Available metadata (arguments) behind `#[graphql]` (or `#[graphql_interface]`) attribute placed
/// on a trait definition, when generating code for [GraphQL interface][1] type.
///
/// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
#[derive(Debug, Default)]
struct InterfaceMeta {
    /// Explicitly specified name of [GraphQL interface][1] type.
    ///
    /// If absent, then Rust type name is used by default.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub name: Option<SpanContainer<String>>,

    /// Explicitly specified [description][2] of [GraphQL interface][1] type.
    ///
    /// If absent, then Rust doc comment is used as [description][2], if any.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    /// [2]: https://spec.graphql.org/June2018/#sec-Descriptions
    pub description: Option<SpanContainer<String>>,

    /// Explicitly specified type of `juniper::Context` to use for resolving this
    /// [GraphQL interface][1] type with.
    ///
    /// If absent, then unit type `()` is assumed as type of `juniper::Context`.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub context: Option<SpanContainer<syn::Type>>,

    /// Explicitly specified type of `juniper::ScalarValue` to use for resolving this
    /// [GraphQL interface][1] type with.
    ///
    /// If absent, then generated code will be generic over any `juniper::ScalarValue` type, which,
    /// in turn, requires all [interface][1] implementers to be generic over any
    /// `juniper::ScalarValue` type too. That's why this type should be specified only if one of the
    /// implementers implements `juniper::GraphQLType` in a non-generic way over
    /// `juniper::ScalarValue` type.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub scalar: Option<SpanContainer<syn::Type>>,

    /// Explicitly specified Rust types of [GraphQL objects][2] implementing this
    /// [GraphQL interface][1] type.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    /// [2]: https://spec.graphql.org/June2018/#sec-Objects
    pub implementers: HashSet<SpanContainer<syn::Type>>,

    /*
    /// Explicitly specified external downcasting functions for [GraphQL interface][1] implementers.
    ///
    /// If absent, then macro will try to auto-infer all the possible variants from the type
    /// declaration, if possible. That's why specifying an external resolver function has sense,
    /// when some custom [union][1] variant resolving logic is involved, or variants cannot be
    /// inferred.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub external_downcasters: InterfaceMetaDowncasters,*/
    /// Indicator whether the generated code is intended to be used only inside the `juniper`
    /// library.
    pub is_internal: bool,
}

impl Parse for InterfaceMeta {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut output = Self::default();

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            match ident.to_string().as_str() {
                "name" => {
                    input.parse::<token::Eq>()?;
                    let name = input.parse::<syn::LitStr>()?;
                    output
                        .name
                        .replace(SpanContainer::new(
                            ident.span(),
                            Some(name.span()),
                            name.value(),
                        ))
                        .none_or_else(|_| dup_attr_err(ident.span()))?
                }
                "desc" | "description" => {
                    input.parse::<token::Eq>()?;
                    let desc = input.parse::<syn::LitStr>()?;
                    output
                        .description
                        .replace(SpanContainer::new(
                            ident.span(),
                            Some(desc.span()),
                            desc.value(),
                        ))
                        .none_or_else(|_| dup_attr_err(ident.span()))?
                }
                "ctx" | "context" | "Context" => {
                    input.parse::<token::Eq>()?;
                    let ctx = input.parse::<syn::Type>()?;
                    output
                        .context
                        .replace(SpanContainer::new(ident.span(), Some(ctx.span()), ctx))
                        .none_or_else(|_| dup_attr_err(ident.span()))?
                }
                "scalar" | "Scalar" | "ScalarValue" => {
                    input.parse::<token::Eq>()?;
                    let scl = input.parse::<syn::Type>()?;
                    output
                        .scalar
                        .replace(SpanContainer::new(ident.span(), Some(scl.span()), scl))
                        .none_or_else(|_| dup_attr_err(ident.span()))?
                }
                "for" | "implementers" => {
                    let inner;
                    syn::parenthesized!(inner in input);
                    while !inner.is_empty() {
                        let impler = inner.parse::<syn::Type>()?;
                        let impler_span = impler.span();
                        output
                            .implementers
                            .replace(SpanContainer::new(ident.span(), Some(impler_span), impler))
                            .none_or_else(|_| dup_attr_err(impler_span))?;
                        inner.try_parse::<token::Comma>()?;
                    }
                }
                "internal" => {
                    output.is_internal = true;
                }
                _ => {
                    return Err(syn::Error::new(ident.span(), "unknown attribute"));
                }
            }
            input.try_parse::<token::Comma>()?;
        }

        Ok(output)
    }
}

impl InterfaceMeta {
    /// Tries to merge two [`InterfaceMeta`]s into a single one, reporting about duplicates, if any.
    fn try_merge(self, mut another: Self) -> syn::Result<Self> {
        Ok(Self {
            name: try_merge_opt!(name: self, another),
            description: try_merge_opt!(description: self, another),
            context: try_merge_opt!(context: self, another),
            scalar: try_merge_opt!(scalar: self, another),
            implementers: try_merge_hashset!(implementers: self, another => span_joined),
            is_internal: self.is_internal || another.is_internal,
        })
    }

    /// Parses [`InterfaceMeta`] from the given multiple `name`d [`syn::Attribute`]s placed on a
    /// trait definition.
    pub fn from_attrs(name: &str, attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut meta = filter_attrs(name, attrs)
            .map(|attr| attr.parse_args())
            .try_fold(Self::default(), |prev, curr| prev.try_merge(curr?))?;

        if meta.description.is_none() {
            meta.description = get_doc_comment(attrs);
        }

        Ok(meta)
    }
}

/// Definition of [GraphQL interface][1] implementer for code generation.
///
/// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
struct InterfaceImplementerDefinition {
    /// Rust type that this [GraphQL interface][1] implementer resolves into.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub ty: syn::Type,

    /// Rust code for downcasting into this [GraphQL interface][1] implementer.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub downcast_code: Option<syn::Expr>,

    /// Rust code for checking whether [GraphQL interface][1] should be downcast into this
    /// implementer.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub downcast_check: Option<syn::Expr>,

    /// Rust type of `juniper::Context` that this [GraphQL interface][1] implementer requires for
    /// downcasting.
    ///
    /// It's available only when code generation happens for Rust traits and a trait method contains
    /// context argument.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub context_ty: Option<syn::Type>,

    /// [`Span`] that points to the Rust source code which defines this [GraphQL interface][1]
    /// implementer.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Unions
    pub span: Span,
}

/// Definition of [GraphQL interface][1] for code generation.
///
/// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
struct InterfaceDefinition {
    /// Name of this [GraphQL interface][1] in GraphQL schema.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub name: String,

    /// Rust type that this [GraphQL interface][1] is represented with.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub ty: syn::Type,

    /// Generics of the Rust type that this [GraphQL interface][1] is implemented for.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub generics: syn::Generics,

    /// Indicator whether code should be generated for a trait object, rather than for a regular
    /// Rust type.
    pub is_trait_object: bool,

    /// Description of this [GraphQL interface][1] to put into GraphQL schema.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub description: Option<String>,

    /// Rust type of `juniper::Context` to generate `juniper::GraphQLType` implementation with
    /// for this [GraphQL interface][1].
    ///
    /// If [`None`] then generated code will use unit type `()` as `juniper::Context`.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub context: Option<syn::Type>,

    /// Rust type of `juniper::ScalarValue` to generate `juniper::GraphQLType` implementation with
    /// for this [GraphQL interface][1].
    ///
    /// If [`None`] then generated code will be generic over any `juniper::ScalarValue` type, which,
    /// in turn, requires all [interface][1] implementers to be generic over any
    /// `juniper::ScalarValue` type too. That's why this type should be specified only if one of the
    /// implementers implements `juniper::GraphQLType` in a non-generic way over
    /// `juniper::ScalarValue` type.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub scalar: Option<syn::Type>,

    /// Implementers definitions of this [GraphQL interface][1].
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub implementers: Vec<InterfaceImplementerDefinition>,
}

impl ToTokens for InterfaceDefinition {
    fn to_tokens(&self, into: &mut TokenStream) {
        let name = &self.name;
        let ty = &self.ty;

        let context = self
            .context
            .as_ref()
            .map(|ctx| quote! { #ctx })
            .unwrap_or_else(|| quote! { () });

        let scalar = self
            .scalar
            .as_ref()
            .map(|scl| quote! { #scl })
            .unwrap_or_else(|| quote! { __S });

        let description = self
            .description
            .as_ref()
            .map(|desc| quote! { .description(#desc) });

        let impler_types: Vec<_> = self.implementers.iter().map(|impler| &impler.ty).collect();

        let all_implers_unique = if impler_types.len() > 1 {
            Some(quote! { ::juniper::sa::assert_type_ne_all!(#(#impler_types),*); })
        } else {
            None
        };

        let custom_downcast_checks = self.implementers.iter().filter_map(|impler| {
            let impler_check = impler.downcast_check.as_ref()?;
            let impler_ty = &impler.ty;

            Some(quote! {
                if #impler_check {
                    return <#impler_ty as ::juniper::GraphQLType<#scalar>>::name(info)
                        .unwrap().to_string();
                }
            })
        });
        let regular_downcast_check = if self.is_trait_object {
            quote! {
                self.as_dyn_graphql_value().concrete_type_name(context, info)
            }
        } else {
            quote! {
                panic!(
                    "GraphQL interface {} cannot be downcast into any of its implementers in its \
                     current state",
                    #name,
                );
            }
        };

        let custom_downcasts = self.implementers.iter().filter_map(|impler| {
            let downcast_code = impler.downcast_code.as_ref()?;
            let impler_ty = &impler.ty;

            let get_name = quote! {
                (<#impler_ty as ::juniper::GraphQLType<#scalar>>::name(info))
            };
            Some(quote! {
                if type_name == #get_name.unwrap() {
                    return ::juniper::IntoResolvable::into(
                        { #downcast_code },
                        executor.context()
                    )
                    .and_then(|res| match res {
                        Some((ctx, r)) => executor.replaced_context(ctx).resolve_with_ctx(info, &r),
                        None => Ok(::juniper::Value::null()),
                    });
                }
            })
        });
        let custom_async_downcasts = self.implementers.iter().filter_map(|impler| {
            let downcast_code = impler.downcast_code.as_ref()?;
            let impler_ty = &impler.ty;

            let get_name = quote! {
                (<#impler_ty as ::juniper::GraphQLType<#scalar>>::name(info))
            };
            Some(quote! {
                if type_name == #get_name.unwrap() {
                    let res = ::juniper::IntoResolvable::into(
                        { #downcast_code },
                        executor.context()
                    );
                    return ::juniper::futures::future::FutureExt::boxed(async move {
                        match res? {
                            Some((ctx, r)) => {
                                let subexec = executor.replaced_context(ctx);
                                subexec.resolve_with_ctx_async(info, &r).await
                            },
                            None => Ok(::juniper::Value::null()),
                        }
                    });
                }
            })
        });
        let (regular_downcast, regular_async_downcast) = if self.is_trait_object {
            let sync = quote! {
                return ::juniper::IntoResolvable::into(
                    self.as_dyn_graphql_value(),
                    executor.context(),
                )
                .and_then(|res| match res {
                    Some((ctx, r)) => executor.replaced_context(ctx).resolve_with_ctx(info, &r),
                    None => Ok(::juniper::Value::null()),
                })
            };
            let r#async = quote! {
                let res = ::juniper::IntoResolvable::into(
                    self.as_dyn_graphql_value(),
                    executor.context()
                );
                return ::juniper::futures::future::FutureExt::boxed(async move {
                    match res? {
                        Some((ctx, r)) => {
                            let subexec = executor.replaced_context(ctx);
                            subexec.resolve_with_ctx_async(info, &r).await
                        },
                        None => Ok(::juniper::Value::null()),
                    }
                });
            };
            (sync, r#async)
        } else {
            let panic = quote! {
                panic!(
                    "Concrete type {} cannot be downcast from on GraphQL interface {}",
                    type_name, #name,
                );
            };
            (panic.clone(), panic)
        };

        let (_, ty_generics, _) = self.generics.split_for_impl();

        let mut ext_generics = self.generics.clone();
        if self.is_trait_object {
            ext_generics.params.push(parse_quote! { '__obj });
        }
        if self.scalar.is_none() {
            ext_generics.params.push(parse_quote! { #scalar });
            ext_generics
                .make_where_clause()
                .predicates
                .push(parse_quote! { #scalar: ::juniper::ScalarValue });
        }
        let (ext_impl_generics, _, where_clause) = ext_generics.split_for_impl();

        let mut where_async = where_clause
            .cloned()
            .unwrap_or_else(|| parse_quote! { where });
        where_async.predicates.push(parse_quote! { Self: Sync });
        if self.scalar.is_none() {
            where_async
                .predicates
                .push(parse_quote! { #scalar: Send + Sync });
        }

        let mut ty_full = quote! { #ty#ty_generics };
        if self.is_trait_object {
            let mut ty_params = None;
            if !self.generics.params.is_empty() {
                let params = &self.generics.params;
                ty_params = Some(quote! { #params, });
            };
            ty_full = quote! {
                dyn #ty<#ty_params #scalar, Context = #context, TypeInfo = ()> +
                    '__obj + Send + Sync
            };
        }

        let type_impl = quote! {
            #[automatically_derived]
            impl#ext_impl_generics ::juniper::GraphQLType<#scalar> for #ty_full
                #where_clause
            {
                fn name(_ : &Self::TypeInfo) -> Option<&'static str> {
                    Some(#name)
                }

                fn meta<'r>(
                    info: &Self::TypeInfo,
                    registry: &mut ::juniper::Registry<'r, #scalar>
                ) -> ::juniper::meta::MetaType<'r, #scalar>
                where #scalar: 'r,
                {
                    // TODO: enumerate implementors
                    // TODO: fields
                    registry.build_interface_type::<#ty_full>(info, &[])
                    #description
                    .into_meta()
                }
            }
        };

        let value_impl = quote! {
            #[automatically_derived]
            impl#ext_impl_generics ::juniper::GraphQLValue<#scalar> for #ty_full
                #where_clause
            {
                type Context = #context;
                type TypeInfo = ();

                fn type_name<'__i>(&self, info: &'__i Self::TypeInfo) -> Option<&'__i str> {
                    <Self as ::juniper::GraphQLType<#scalar>>::name(info)
                }

                fn concrete_type_name(
                    &self,
                    context: &Self::Context,
                    info: &Self::TypeInfo,
                ) -> String {
                    #( #custom_downcast_checks )*
                    #regular_downcast_check
                }

                fn resolve_into_type(
                    &self,
                    info: &Self::TypeInfo,
                    type_name: &str,
                    _: Option<&[::juniper::Selection<#scalar>]>,
                    executor: &::juniper::Executor<Self::Context, #scalar>,
                ) -> ::juniper::ExecutionResult<#scalar> {
                    let context = executor.context();
                    #( #custom_downcasts )*
                    #regular_downcast
                }
            }
        };

        let value_async_impl = quote! {
            #[automatically_derived]
            impl#ext_impl_generics ::juniper::GraphQLValueAsync<#scalar> for #ty_full
                #where_async
            {
                fn resolve_into_type_async<'b>(
                    &'b self,
                    info: &'b Self::TypeInfo,
                    type_name: &str,
                    _: Option<&'b [::juniper::Selection<'b, #scalar>]>,
                    executor: &'b ::juniper::Executor<'b, 'b, Self::Context, #scalar>
                ) -> ::juniper::BoxFuture<'b, ::juniper::ExecutionResult<#scalar>> {
                    let context = executor.context();
                    #( #custom_async_downcasts )*
                    #regular_async_downcast
                }
            }
        };

        let output_type_impl = quote! {
            #[automatically_derived]
            impl#ext_impl_generics ::juniper::marker::IsOutputType<#scalar> for #ty_full
                #where_clause
            {
                fn mark() {
                    #( <#impler_types as ::juniper::marker::GraphQLObjectType<#scalar>>::mark(); )*
                }
            }
        };

        let interface_impl = quote! {
            #[automatically_derived]
            impl#ext_impl_generics ::juniper::marker::GraphQLInterface<#scalar> for #ty_full
                #where_clause
            {
                fn mark() {
                    #all_implers_unique

                    #( <#impler_types as ::juniper::marker::GraphQLObjectType<#scalar>>::mark(); )*
                }
            }
        };

        into.append_all(&[
            interface_impl,
            output_type_impl,
            type_impl,
            value_impl,
            value_async_impl,
        ]);
    }
}
