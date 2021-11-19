use crate::build_in_types::BuiltInType;
use crate::errors::{ParseError, ParseErrors};
use crate::{BridgedType, ParsedExternFn, SwiftBridgeModule};
use proc_macro2::Ident;
use quote::ToTokens;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Deref;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{FnArg, ForeignItem, ForeignItemFn, Item, ItemMod, Pat, ReturnType, Token, Type};

// TODO: (11/18/21) Tomorrow morning do some refactoring while running test suite to make sure we
//  didn't break anything.
//  This should normalize a bunch of stuff and make it easy to continue to layer on functionality.
//  And make sure we consistently handle things like treating `&self` vs `self: &Foo` as the same
//  thing.

impl Parse for SwiftBridgeModule {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let module_and_errors: SwiftBridgeModuleAndErrors = input.parse()?;

        module_and_errors.errors.combine_all()?;

        Ok(module_and_errors.module)
    }
}

pub(crate) struct SwiftBridgeModuleAndErrors {
    pub module: SwiftBridgeModule,
    pub errors: ParseErrors,
}

/// The language that a bridge type or function's implementation lives in.
#[derive(Debug, Copy, Clone)]
pub(crate) enum HostLang {
    /// The type or function is defined Rust.
    Rust,
    /// The type or function is defined Swift.
    Swift,
}

impl HostLang {
    pub fn is_rust(&self) -> bool {
        matches!(self, HostLang::Rust)
    }

    pub fn is_swift(&self) -> bool {
        matches!(self, HostLang::Swift)
    }
}

impl Parse for SwiftBridgeModuleAndErrors {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut errors = ParseErrors::new();

        if let Ok(item_mod) = input.parse::<ItemMod>() {
            let module_name = item_mod.ident;

            let mut functions = vec![];
            let mut all_type_declarations = HashMap::new();

            for outer_mod_item in item_mod.content.unwrap().1 {
                match outer_mod_item {
                    Item::ForeignMod(mut foreign_mod) => {
                        if foreign_mod.abi.name.is_none() {
                            errors.push(ParseError::AbiNameMissing {
                                extern_token: foreign_mod.abi.extern_token,
                            });
                            continue;
                        }

                        let mut local_type_declarations = HashMap::new();

                        let abi_name = foreign_mod.abi.name.unwrap();

                        let host_lang = match abi_name.value().as_str() {
                            "Rust" => HostLang::Rust,
                            "Swift" => HostLang::Swift,
                            _ => {
                                errors.push(ParseError::AbiNameInvalid { abi_name });
                                continue;
                            }
                        };

                        foreign_mod.items.sort_by(|a, _b| {
                            if matches!(a, ForeignItem::Type(_)) {
                                Ordering::Less
                            } else {
                                Ordering::Greater
                            }
                        });

                        for foreign_mod_item in foreign_mod.items {
                            match foreign_mod_item {
                                ForeignItem::Type(foreign_ty) => {
                                    let ty_name = foreign_ty.ident.to_string();

                                    if let Some(_builtin) =
                                        BuiltInType::with_str(&foreign_ty.ident.to_string())
                                    {
                                        errors.push(ParseError::DeclaredBuiltInType {
                                            ty: foreign_ty.clone(),
                                        });
                                    }

                                    let bridged_type = BridgedType {
                                        ty: foreign_ty.clone(),
                                        host_lang,
                                    };
                                    all_type_declarations
                                        .insert(ty_name.clone(), bridged_type.clone());
                                    local_type_declarations.insert(ty_name, bridged_type);
                                }
                                ForeignItem::Fn(func) => {
                                    let mut attributes = SwiftBridgeAttributes::default();

                                    for attr in func.attrs.iter() {
                                        let attr: SwiftBridgeAttr = attr.parse_args()?;
                                        attributes.store_attrib(attr);
                                    }

                                    for arg in func.sig.inputs.iter() {
                                        if let FnArg::Typed(pat_ty) = arg {
                                            check_supported_type(
                                                &pat_ty.ty,
                                                &mut all_type_declarations,
                                                &mut errors,
                                            );
                                        }
                                    }

                                    if let ReturnType::Type(_, ty) = &func.sig.output {
                                        check_supported_type(
                                            ty,
                                            &mut all_type_declarations,
                                            &mut errors,
                                        );
                                    }

                                    let first_input = func.sig.inputs.iter().next();

                                    if let Some(first) = first_input {
                                        parse_function_with_inputs(
                                            first,
                                            func.clone(),
                                            &attributes,
                                            host_lang,
                                            &mut all_type_declarations,
                                            &mut local_type_declarations,
                                            &mut functions,
                                            &mut errors,
                                        )?;
                                    } else {
                                        let f = parse_function(
                                            func.clone(),
                                            &attributes,
                                            host_lang,
                                            &mut all_type_declarations,
                                            &mut errors,
                                        );
                                        functions.push(f);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {
                        //
                        todo!("Push an error that the module may only contain `extern` blocks.")
                    }
                };
            }

            let module = SwiftBridgeModule {
                name: module_name,
                types: all_type_declarations.into_values().collect(),
                functions,
            };
            Ok(SwiftBridgeModuleAndErrors { module, errors })
        } else {
            return Err(syn::Error::new_spanned(
                input.to_string(),
                "Only modules and impl blocks are supported.",
            ));
        }
    }
}

fn check_supported_type(
    ty: &Type,
    type_lookup: &mut HashMap<String, BridgedType>,
    errors: &mut ParseErrors,
) {
    let (ty_string, ty_span) = match ty.deref() {
        Type::Path(path) => (path.path.to_token_stream().to_string(), path.path.span()),
        Type::Reference(ref_ty) => (
            ref_ty.elem.to_token_stream().to_string(),
            ref_ty.elem.span(),
        ),
        Type::Ptr(ptr) => (ptr.elem.to_token_stream().to_string(), ptr.elem.span()),
        _ => todo!("Handle other type possibilities"),
    };

    if !type_lookup.contains_key(&ty_string) && BuiltInType::with_type(ty).is_none() {
        errors.push(ParseError::UndeclaredType {
            ty: ty_string,
            span: ty_span,
        });
    }
}

// Parse a freestanding or associated function.
fn parse_function(
    func: ForeignItemFn,
    attributes: &SwiftBridgeAttributes,
    host_lang: HostLang,
    type_lookup: &mut HashMap<String, BridgedType>,
    errors: &mut ParseErrors,
) -> ParsedExternFn {
    let associated_type = if let Some(associated_to) = &attributes.associated_to {
        let ty = type_lookup.get_mut(&associated_to.to_string()).unwrap();
        Some(ty.clone())
    } else if attributes.initializes {
        let ty_string = match &func.sig.output {
            ReturnType::Default => {
                todo!("Push error if initializer does not return a type")
            }
            ReturnType::Type(_, ty) => ty.deref().to_token_stream().to_string(),
        };

        let ty = type_lookup.get_mut(&ty_string);

        ty.map(|ty| ty.clone())
    } else {
        None
    };

    ParsedExternFn {
        func,
        is_initializer: attributes.initializes,
        host_lang,
        associated_type,
    }
}

// Parse a function that has inputs (i.e. perhaps self or arguments)
fn parse_function_with_inputs(
    first: &FnArg,
    func: ForeignItemFn,
    attributes: &SwiftBridgeAttributes,
    host_lang: HostLang,
    all_type_declarations: &mut HashMap<String, BridgedType>,
    local_type_declarations: &mut HashMap<String, BridgedType>,
    functions: &mut Vec<ParsedExternFn>,
    errors: &mut ParseErrors,
) -> syn::Result<()> {
    match first {
        FnArg::Receiver(recv) => {
            if local_type_declarations.len() == 1 {
                let ty = local_type_declarations.iter_mut().next().unwrap().1;
                functions.push(ParsedExternFn {
                    func,
                    is_initializer: attributes.initializes,
                    associated_type: Some(ty.clone()),
                    host_lang,
                });
            } else {
                errors.push(ParseError::AmbiguousSelf {
                    self_: recv.clone(),
                });
            }
        }
        FnArg::Typed(arg) => {
            match arg.pat.deref() {
                Pat::Ident(pat_ident) => {
                    if pat_ident.ident.to_string() == "self" {
                        let self_ty = arg.ty.deref();

                        match self_ty {
                            Type::Path(_path) => {
                                parse_method(
                                    func.clone(),
                                    attributes,
                                    host_lang,
                                    all_type_declarations,
                                    functions,
                                    self_ty,
                                );
                            }
                            Type::Reference(type_ref) => {
                                let self_ty = type_ref.elem.deref();

                                parse_method(
                                    func.clone(),
                                    attributes,
                                    host_lang,
                                    all_type_declarations,
                                    functions,
                                    self_ty,
                                );
                            }
                            _ => {}
                        };
                    } else if let Some(associated_to) = &attributes.associated_to {
                        let f = parse_function(
                            func,
                            attributes,
                            host_lang,
                            all_type_declarations,
                            errors,
                        );
                        functions.push(f);
                    } else if attributes.initializes {
                        let f = parse_function(
                            func,
                            attributes,
                            host_lang,
                            all_type_declarations,
                            errors,
                        );
                        functions.push(f);
                    } else {
                        functions.push(ParsedExternFn {
                            func,
                            is_initializer: false,
                            associated_type: None,
                            host_lang,
                        });
                    }
                }
                _ => {}
            };
        }
    };

    Ok(())
}

// Parse a function that has `self`
fn parse_method(
    func: ForeignItemFn,
    attributes: &SwiftBridgeAttributes,
    host_lang: HostLang,
    type_lookup: &mut HashMap<String, BridgedType>,
    functions: &mut Vec<ParsedExternFn>,
    self_ty: &Type,
) {
    match self_ty {
        Type::Path(path) => {
            let self_ty_string = path.path.segments.to_token_stream().to_string();

            if let Some(ty) = type_lookup.get_mut(&self_ty_string) {
                functions.push(ParsedExternFn {
                    func,
                    is_initializer: attributes.initializes,
                    associated_type: Some(ty.clone()),
                    host_lang,
                });
            } else {
                todo!("Handle type not present in this extern push pushing a ParseError")
            }
        }
        _ => {
            todo!("Add a test that hits this block..")
        }
    };
}

#[derive(Default)]
struct SwiftBridgeAttributes {
    associated_to: Option<Ident>,
    initializes: bool,
}

impl SwiftBridgeAttributes {
    fn store_attrib(&mut self, attrib: SwiftBridgeAttr) {
        match attrib {
            SwiftBridgeAttr::AssociatedTo(ident) => {
                self.associated_to = Some(ident);
            }
            SwiftBridgeAttr::Init => self.initializes = true,
        }
    }
}

enum SwiftBridgeAttr {
    AssociatedTo(Ident),
    Init,
}

impl Parse for SwiftBridgeAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let key: Ident = input.parse()?;

        let attrib = match key.to_string().as_str() {
            "associated_to" => {
                input.parse::<Token![=]>()?;
                let value: Ident = input.parse()?;

                SwiftBridgeAttr::AssociatedTo(value)
            }
            "init" => SwiftBridgeAttr::Init,
            _ => panic!("TODO: Return spanned error"),
        };

        Ok(attrib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ParseError;
    use proc_macro2::TokenStream;
    use quote::quote;
    use syn::parse_quote;

    /// Verify that we can parse a SwiftBridgeModule from an empty module.
    #[test]
    fn parse_empty_module() {
        let tokens = quote! {
            mod foo { }
        };
        let module: SwiftBridgeModule = parse_quote!(#tokens);

        assert_eq!(module.name.to_string(), "foo");
    }

    /// Verify that we store an error if no abi name was provided.
    #[test]
    fn error_if_no_abi_name_provided_for_an_extern_block() {
        let tokens = quote! {
            mod foo {
                extern {}
            }
        };
        let errors = parse_errors(tokens);

        assert_eq!(errors.len(), 1);
        match errors[0] {
            ParseError::AbiNameMissing { .. } => {}
            _ => panic!(),
        }
    }

    /// Verify that we store an error if the abi name isn't Rust or Swift.
    #[test]
    fn error_if_invalid_abi_name() {
        let tokens = quote! {
            mod foo {
                extern "SomeAbi" {}
            }
        };
        let errors = parse_errors(tokens);

        assert_eq!(errors.len(), 1);
        match &errors[0] {
            ParseError::AbiNameInvalid { abi_name } => {
                assert_eq!(abi_name.value(), "SomeAbi");
            }
            _ => panic!(),
        }
    }

    /// Verify that we can parse a Rust type declaration.
    #[test]
    fn rust_type_declaration() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;
                }
            }
        };

        let module = parse_ok(tokens);

        assert_eq!(module.types[0].ident.to_string(), "Foo");
    }

    /// Verify that we return an error if the declared type is a built in type.
    #[test]
    fn error_if_declared_built_in_type() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type u8;
                }
            }
        };

        let errors = parse_errors(tokens);
        assert_eq!(errors.len(), 1);
    }

    /// Verify that we can parse a Rust type's methods.
    /// We test all of the possible ways we can specify self.
    #[test]
    fn parses_rust_self_methods() {
        let tests = vec![
            quote! { fn bar (self); },
            quote! { fn bar (&self); },
            quote! { fn bar (&mut self); },
            quote! { fn bar (self: Foo); },
            quote! { fn bar (self: &Foo); },
            quote! { fn bar (self: &mut Foo); },
        ];

        for fn_definition in tests {
            let tokens = quote! {
                mod foo {
                    extern "Rust" {
                        type Foo;

                        #fn_definition
                    }
                }
            };

            let module = parse_ok(tokens);

            let ty = &module.types[0];
            assert_eq!(ty.ident.to_string(), "Foo");

            assert_eq!(
                module.functions.len(),
                1,
                "Failed not parse {} into an associated method.",
                quote! {#fn_definition}.to_string()
            );
        }
    }

    /// Verify that we can parse an associated function.
    #[test]
    fn parse_associated_function() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    #[swift_bridge(associated_to = Foo)]
                    fn bar ();
                }
            }
        };

        let module = parse_ok(tokens);

        let ty = &module.types[0];
        assert_eq!(ty.ident.to_string(), "Foo");

        assert_eq!(module.functions.len(), 1,);
    }

    /// Verify that we can parse an associated function that has arguments.
    #[test]
    fn associated_function_with_args() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    #[swift_bridge(associated_to = Foo)]
                    fn bar (arg: u8);
                }
            }
        };

        let module = parse_ok(tokens);

        let ty = &module.types[0];
        assert_eq!(ty.ident.to_string(), "Foo");

        assert_eq!(module.functions.len(), 1,);
    }

    /// Verify that we can parse an init function.
    #[test]
    fn initializer() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    #[swift_bridge(init)]
                    fn bar () -> Foo;
                }
            }
        };

        let module = parse_ok(tokens);

        let func = &module.functions[0];
        assert!(func.is_initializer);
    }

    /// Verify that we can parse an init function that takes inputs.
    #[test]
    fn initializer_with_inputs() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    #[swift_bridge(init)]
                    fn bar (bazz: u8) -> Foo;
                }
            }
        };

        let module = parse_ok(tokens);

        let func = &module.functions[0];
        assert!(func.is_initializer);
    }

    /// Verify that we push an error if the initialize type is not defined.
    #[test]
    fn error_if_initialized_type_not_defined() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    #[swift_bridge(init)]
                    fn bar () -> Foo;
                }
            }
        };

        let errors = parse_errors(tokens);
        assert_eq!(errors.len(), 1);

        match &errors[0] {
            ParseError::UndeclaredType { ty, span: _ } => {
                assert_eq!(ty.to_string(), "Foo")
            }
            _ => panic!(),
        }
    }

    /// Verify that we can parse a freestanding Rust function declaration.
    #[test]
    fn rust_freestanding_function_no_args() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    fn bar () -> u8;
                }
            }
        };

        let module = parse_ok(tokens);

        assert_eq!(module.functions.len(), 1);
    }

    /// Verify that we can parse a freestanding Rust function declaration that has one arg.
    #[test]
    fn rust_freestanding_function_one_arg() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    fn bar (bazz: u32) -> u8;
                }
            }
        };

        let module = parse_ok(tokens);

        assert_eq!(module.functions.len(), 1);
    }

    /// Verify that if a freestanding function has argument types that were not declared in the
    /// module we return an error.
    #[test]
    fn freestanding_function_argument_undeclared_type() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    fn a (bar: Bar);
                    fn b (bar: &Bar);
                    fn c (bar: &mut Bar);
                    // Counts as two errors.
                    fn d (multiple: Bar, args: Bar);
                }
            }
        };
        let errors = parse_errors(tokens);
        assert_eq!(errors.len(), 5);

        for error in errors.iter() {
            match error {
                ParseError::UndeclaredType { ty, span: _ } => {
                    assert_eq!(ty, "Bar");
                }
                _ => panic!(),
            }
        }
    }

    /// Verify that a freestanding function can return a declared type.
    #[test]
    fn freestanding_function_return_declared_type() {
        let tokens = quote! {
            #[swift_bridge::bridge]
            mod foo {
                extern "Rust" {
                    type Bar;

                    fn a () -> Bar;
                }
            }
        };
        let module = parse_ok(tokens);
        assert_eq!(module.functions.len(), 1);
    }

    /// Verify that if a freestanding function returns a type that was not declared in the module
    /// we return an error.
    #[test]
    fn freestanding_function_returns_undeclared_type() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type Foo;

                    fn a () -> Bar;
                    fn a () -> &Bar;
                    fn a () -> &mut Bar;
                }
            }
        };
        let errors = parse_errors(tokens);
        assert_eq!(errors.len(), 3);

        for error in errors.iter() {
            match error {
                ParseError::UndeclaredType { ty, span: _ } => {
                    assert_eq!(ty, "Bar");
                }
                _ => panic!(),
            }
        }
    }

    /// Verify that if an extern Rust block has more than one type, we push errors for any methods
    /// that have an ambiguous self.
    #[test]
    fn error_if_method_has_ambiguous_self() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type SomeType;
                    type AnotherType;

                    fn a (self);
                    fn b (&self);
                    fn c (&mut self);
                }
            }
        };

        let errors = parse_errors(tokens);

        assert_eq!(errors.len(), 3);

        for idx in 0..3 {
            match &errors[idx] {
                ParseError::AmbiguousSelf { self_: _ } => {}
                _ => panic!(),
            };
        }
    }

    /// Verify that annotated self methods get parsed.
    #[test]
    fn disambiguate_method() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type SomeType;
                    type AnotherType;

                    fn a (self: SomeType);
                    fn b (self: &SomeType);
                    fn c (self: &mut AnotherType);
                }
            }
        };

        let mut module = parse_ok(tokens);

        let types = &mut module.types;
        types.sort_by(|a, b| a.ident.to_string().cmp(&b.ident.to_string()).reverse());

        let functions = &module.functions;

        for (ty_name, expected_count) in vec![("SomeType", 2), ("AnotherType", 1)] {
            assert_eq!(
                functions
                    .iter()
                    .filter(|f| f.associated_type.as_ref().unwrap().ident == ty_name)
                    .count(),
                expected_count
            );
        }
    }

    /// Verify that if we have multiple externs types can be inferred within each.
    #[test]
    fn infer_type_with_multiple_externs() {
        let tokens = quote! {
            mod foo {
                extern "Rust" {
                    type SomeType;

                    fn a (&self);
                }

                extern "Rust" {
                    type AnotherType;

                    fn b (&self);
                }
            }
        };

        let mut module = parse_ok(tokens);

        let types = &mut module.types;
        types.sort_by(|a, b| a.ident.to_string().cmp(&b.ident.to_string()).reverse());

        let functions = &module.functions;

        for (ty_name, expected_count) in vec![("SomeType", 1), ("AnotherType", 1)] {
            assert_eq!(
                functions
                    .iter()
                    .filter(|f| f.associated_type.as_ref().unwrap().ident == ty_name)
                    .count(),
                expected_count
            );
        }
    }

    fn parse_ok(tokens: TokenStream) -> SwiftBridgeModule {
        let parsed: SwiftBridgeModuleAndErrors = parse_quote!(#tokens);
        parsed.module
    }

    fn parse_errors(tokens: TokenStream) -> ParseErrors {
        let parsed: SwiftBridgeModuleAndErrors = syn::parse2(tokens).unwrap();
        parsed.errors
    }
}
