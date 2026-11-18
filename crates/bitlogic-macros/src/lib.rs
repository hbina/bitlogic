//! Pure structured Rust functions lowered to combinational expression graphs.
mod collections;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use std::collections::BTreeMap;
use syn::{Expr, FnArg, GenericArgument, ItemFn, Pat, PathArguments, Stmt, Type, spanned::Spanned};

fn error(node: impl Spanned, message: &str) -> syn::Error {
    syn::Error::new(node.span(), message)
}
fn report(result: syn::Result<Tokens>) -> TokenStream {
    result.unwrap_or_else(|e| e.into_compile_error()).into()
}
#[proc_macro_attribute]
pub fn logic(attribute: TokenStream, item: TokenStream) -> TokenStream {
    if !attribute.is_empty() {
        return error(Tokens::from(attribute), "#[logic] takes no arguments")
            .into_compile_error()
            .into();
    }
    report(syn::parse::<ItemFn>(item).and_then(expand))
}

/// `fixed_set!(pub mod name: Key, capacity = N)` generates a module with a
/// fixed-capacity `Set` and its `#[logic]` operations.
#[proc_macro]
pub fn fixed_set(item: TokenStream) -> TokenStream {
    report(syn::parse::<collections::Spec>(item).and_then(|s| collections::expand(s, false)))
}
/// `fixed_map!(pub mod name: Key => Value, capacity = N)` generates a module
/// with a fixed-capacity `Map` and its `#[logic]` operations.
#[proc_macro]
pub fn fixed_map(item: TokenStream) -> TokenStream {
    report(syn::parse::<collections::Spec>(item).and_then(|s| collections::expand(s, true)))
}

#[proc_macro_derive(LogicValue)]
pub fn logic_value(item: TokenStream) -> TokenStream {
    report(syn::parse::<syn::DeriveInput>(item).and_then(derive_value))
}
fn derive_value(input: syn::DeriveInput) -> syn::Result<Tokens> {
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(error(
            input,
            "LogicValue requires a non-generic named struct",
        ));
    }
    if let syn::Data::Enum(data) = &input.data {
        return derive_enum(&input.ident, data);
    }
    let syn::Data::Struct(data) = &input.data else {
        return Err(error(
            input,
            "LogicValue requires a named struct or fieldless enum",
        ));
    };
    let syn::Fields::Named(fields) = &data.fields else {
        return Err(error(input, "LogicValue requires named fields"));
    };
    if fields.named.is_empty() {
        return Err(error(fields, "LogicValue requires at least one field"));
    }
    let name = &input.ident;
    let names: Vec<_> = fields
        .named
        .iter()
        .map(|f| f.ident.as_ref().unwrap())
        .collect();
    let labels: Vec<_> = names.iter().map(|n| clean(n)).collect();
    let types: Vec<_> = fields.named.iter().map(|f| &f.ty).collect();
    for ty in &types {
        value_type(ty)?;
    }
    Ok(quote! {
        impl ::bitlogic::LogicValue for #name {
            fn schema() -> ::bitlogic::Schema {
                ::bitlogic::Schema::structure::<Self>(vec![#((#labels.into(), <#types as ::bitlogic::LogicValue>::schema())),*])
            }
            fn flatten(&self) -> ::std::vec::Vec<u64> {
                let mut values = ::std::vec::Vec::new();
                #(values.extend(::bitlogic::LogicValue::flatten(&self.#names));)*
                values
            }
            fn read(values: &mut impl ::std::iter::Iterator<Item=u64>) -> ::std::result::Result<Self,::bitlogic::Error> {
                Ok(Self { #(#names: <#types as ::bitlogic::LogicValue>::read(values)?),* })
            }
        }
    })
}
/// Fieldless enums are stored as their discriminant. Discriminants are
/// implicit (previous + 1, starting at 0) or integer literals.
fn derive_enum(name: &syn::Ident, data: &syn::DataEnum) -> syn::Result<Tokens> {
    if data.variants.is_empty() {
        return Err(error(name, "LogicValue enums need at least one variant"));
    }
    let mut next = 0u64;
    let mut idents = Vec::new();
    let mut labels = Vec::new();
    let mut values = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, syn::Fields::Unit) {
            return Err(error(variant, "LogicValue enums must be fieldless"));
        }
        let value = match &variant.discriminant {
            Some((_, expr)) => integer(expr)?,
            None => next,
        };
        if values.contains(&value) {
            return Err(error(variant, "duplicate enum discriminant"));
        }
        next = value
            .checked_add(1)
            .ok_or_else(|| error(variant, "enum discriminant overflow"))?;
        idents.push(&variant.ident);
        labels.push(clean(&variant.ident));
        values.push(value);
    }
    Ok(quote! {
        impl ::bitlogic::LogicValue for #name {
            fn schema() -> ::bitlogic::Schema {
                ::bitlogic::Schema::enumeration::<Self>(vec![#((#labels.into(), #values)),*])
            }
            fn flatten(&self) -> ::std::vec::Vec<u64> {
                ::std::vec![match self { #(Self::#idents => #values),* }]
            }
            fn read(values: &mut impl ::std::iter::Iterator<Item=u64>) -> ::std::result::Result<Self,::bitlogic::Error> {
                match values.next() {
                    #(Some(#values) => Ok(Self::#idents),)*
                    _ => Err(::bitlogic::Error(concat!("invalid ", stringify!(#name), " discriminant").into())),
                }
            }
        }
    })
}
fn clean(name: &syn::Ident) -> String {
    name.to_string().trim_start_matches("r#").to_owned()
}
fn value_type(ty: &Type) -> syn::Result<()> {
    match ty {
        Type::Path(p) if p.qself.is_none() => Ok(()),
        Type::Array(a) => value_type(&a.elem),
        Type::Paren(p) => value_type(&p.elem),
        _ => Err(error(
            ty,
            "logic values must be owned bool, Bits<N>, arrays, or derived structs/enums",
        )),
    }
}
fn integer(expr: &Expr) -> syn::Result<u64> {
    if let Expr::Lit(lit) = expr
        && let syn::Lit::Int(n) = &lit.lit
    {
        return n.base10_parse();
    }
    Err(error(expr, "expected an unsigned integer literal"))
}
fn generic_integers(arguments: &PathArguments) -> syn::Result<Vec<u64>> {
    let PathArguments::AngleBracketed(args) = arguments else {
        return Err(error(arguments, "expected literal const arguments"));
    };
    args.args
        .iter()
        .map(|arg| match arg {
            GenericArgument::Const(expr) => integer(expr),
            _ => Err(error(arg, "expected literal const argument")),
        })
        .collect()
}

type Env = BTreeMap<String, Tokens>;
#[derive(Default)]
struct Lower {
    statements: Vec<Tokens>,
    serial: usize,
    work: usize,
}
impl Lower {
    fn emit(&mut self, expr: Tokens) -> Tokens {
        let index = self.serial;
        self.serial += 1;
        self.statements.push(quote!(__logic_values.push(#expr);));
        quote!(__logic_values[#index])
    }
    fn tick(&mut self, node: impl Spanned) -> syn::Result<()> {
        self.work += 1;
        if self.work > 65_536 {
            return Err(error(node, "logic exceeds 65536 lowering steps"));
        }
        Ok(())
    }
    fn block(&mut self, block: &syn::Block, env: &Env) -> syn::Result<Tokens> {
        let mut env = env.clone();
        let Some((last, initial)) = block.stmts.split_last() else {
            return Err(error(block, "expected a trailing value expression"));
        };
        for stmt in initial {
            self.tick(stmt)?;
            let Stmt::Local(local) = stmt else {
                return Err(error(
                    stmt,
                    "only immutable let bindings and a trailing expression are supported",
                ));
            };
            let (pat, annotation) = if let Pat::Type(p) = &local.pat {
                value_type(&p.ty)?;
                (&*p.pat, Some(&p.ty))
            } else {
                (&local.pat, None)
            };
            let binding = immutable_binding(pat)?;
            let Some(init) = &local.init else {
                return Err(error(local, "local bindings require an initializer"));
            };
            if init.diverge.is_some() {
                return Err(error(local, "let-else is not supported"));
            }
            let value = self.expr(&init.expr, &env)?;
            if let Some(ty) = annotation {
                self.statements.push(quote!(#value.require::<#ty>()?;));
            }
            // Immutable shadowing creates a fresh value, as in native Rust.
            env.insert(clean(binding), value);
        }
        let Stmt::Expr(expr, None) = last else {
            return Err(error(last, "expected a trailing value expression"));
        };
        self.expr(expr, &env)
    }
    /// Inline a call to another `#[logic]` function. The callee's macro
    /// expansion provides `<path>::__lower`, which builds its graph into the
    /// caller's builder; the native call is ordinary Rust.
    fn call(&mut self, call: &syn::ExprCall, path: &syn::Path, env: &Env) -> syn::Result<Tokens> {
        if path
            .segments
            .iter()
            .any(|s| !matches!(s.arguments, PathArguments::None))
        {
            return Err(error(
                path,
                "#[logic] functions are non-generic; remove the generic arguments",
            ));
        }
        let mut args = Vec::new();
        for arg in &call.args {
            args.push(self.expr(arg, env)?);
        }
        Ok(self.emit(quote!(#path::__lower(
            __logic_builder,
            ::std::vec![#(#args.clone()),*],
            __logic_depth + 1,
        )?)))
    }
    /// `match` on a scalar (enum, `Bits` or `bool`) with constant patterns:
    /// enum variants, `const`s, `true`/`false`, `|` alternatives and `_`.
    /// Arms are tried in order; rustc's exhaustiveness check on the native
    /// function makes the last arm the default.
    fn matching(&mut self, m: &syn::ExprMatch, env: &Env) -> syn::Result<Tokens> {
        let scrutinee = self.expr(&m.expr, env)?;
        let mut arms = Vec::new();
        for arm in &m.arms {
            if let Some((r#if, _)) = &arm.guard {
                return Err(error(r#if, "match guards are not supported"));
            }
            let mut patterns = Vec::new();
            let wildcard = self.patterns(&arm.pat, env, &mut patterns)?;
            let value = self.expr(&arm.body, env)?;
            arms.push((patterns, wildcard, value));
            if wildcard {
                break;
            }
        }
        let Some((_, _, mut result)) = arms.pop() else {
            return Err(error(m, "match needs at least one arm"));
        };
        for (patterns, _, value) in arms.into_iter().rev() {
            let mut hit: Option<Tokens> = None;
            for pattern in patterns {
                let eq = self.emit(quote!(#scrutinee.binary(__logic_builder,::bitlogic::Binary::Eq,&#pattern,false)?));
                hit = Some(match hit {
                    None => eq,
                    Some(h) => self
                        .emit(quote!(#h.binary(__logic_builder,::bitlogic::Binary::Or,&#eq,true)?)),
                });
            }
            let hit = hit.expect("arm has a pattern");
            result = self.emit(
                quote!(::bitlogic::__private::Symbolic::mux(__logic_builder,&#hit,&#value,&#result)?),
            );
        }
        Ok(result)
    }
    /// Collect the constant values of a pattern; true for `_`.
    fn patterns(&mut self, pat: &Pat, env: &Env, out: &mut Vec<Tokens>) -> syn::Result<bool> {
        self.tick(pat)?;
        match pat {
            Pat::Wild(_) => Ok(true),
            Pat::Or(or) => {
                let mut wildcard = false;
                for case in &or.cases {
                    wildcard |= self.patterns(case, env, out)?;
                }
                Ok(wildcard)
            }
            Pat::Paren(p) => self.patterns(&p.pat, env, out),
            Pat::Path(p) if p.qself.is_none() => {
                out.push(
                    self.emit(
                        quote!(::bitlogic::__private::Symbolic::constant(__logic_builder, #p)?),
                    ),
                );
                Ok(false)
            }
            Pat::Lit(lit) if matches!(lit.lit, syn::Lit::Bool(_)) => {
                out.push(self.emit(
                    quote!(::bitlogic::__private::Symbolic::constant(__logic_builder, #lit)?),
                ));
                Ok(false)
            }
            // A bare name is a `const` (or an imported variant); a binding
            // would be a fresh variable, which the language does not support.
            Pat::Ident(p)
                if p.by_ref.is_none()
                    && p.mutability.is_none()
                    && p.subpat.is_none()
                    && !env.contains_key(&clean(&p.ident)) =>
            {
                let ident = &p.ident;
                out.push(self.emit(
                    quote!(::bitlogic::__private::Symbolic::constant(__logic_builder, #ident)?),
                ));
                Ok(false)
            }
            _ => Err(error(
                pat,
                "match patterns must be enum variants, consts, true/false, | alternatives or _",
            )),
        }
    }
    fn expr(&mut self, expr: &Expr, env: &Env) -> syn::Result<Tokens> {
        self.tick(expr)?;
        match expr {
            Expr::Paren(p) => self.expr(&p.expr, env),
            Expr::Group(g) => self.expr(&g.expr, env),
            Expr::Block(b) if b.label.is_none() => self.block(&b.block, env),
            Expr::Path(p) if p.qself.is_none() => {
                if let Some(ident) = p.path.get_ident()
                    && let Some(value) = env.get(&clean(ident))
                {
                    return Ok(value.clone());
                }
                // Anything else (an enum variant, a `const`) is a native
                // constant of some LogicValue type.
                Ok(self
                    .emit(quote!(::bitlogic::__private::Symbolic::constant(__logic_builder, #p)?)))
            }
            Expr::Index(index) => {
                let base = self.expr(&index.expr, env)?;
                let Ok(i) = integer(&index.index) else {
                    return Err(error(
                        index,
                        "index arrays with an integer literal, or use .at(index) for a run-time index",
                    ));
                };
                let i =
                    usize::try_from(i).map_err(|_| error(index, "array index out of bounds"))?;
                Ok(self.emit(quote!(#base.element(#i)?)))
            }
            Expr::Array(array) => {
                let mut elements = Vec::new();
                for element in &array.elems {
                    let value = self.expr(element, env)?;
                    elements.push(quote!(#value.clone()));
                }
                Ok(self.emit(quote!(::bitlogic::__private::Symbolic::array(
                    ::std::vec![#(#elements),*]
                )?)))
            }
            Expr::Repeat(repeat) => {
                let value = self.expr(&repeat.expr, env)?;
                let len = usize::try_from(integer(&repeat.len)?)
                    .map_err(|_| error(repeat, "array length too large"))?;
                Ok(self.emit(quote!(#value.repeat(#len)?)))
            }
            Expr::Match(m) => self.matching(m, env),
            Expr::Field(f) => {
                let base = self.expr(&f.base, env)?;
                let syn::Member::Named(field) = &f.member else {
                    return Err(error(f, "use named struct fields"));
                };
                let name = clean(field);
                Ok(self.emit(quote!(#base.field(#name)?)))
            }
            Expr::Struct(s) if s.rest.is_none() && s.qself.is_none() => {
                let mut fields = Vec::new();
                for field in &s.fields {
                    let syn::Member::Named(name) = &field.member else {
                        return Err(error(field, "use named struct fields"));
                    };
                    let name = clean(name);
                    let value = self.expr(&field.expr, env)?;
                    fields.push(quote!((#name, #value.clone())));
                }
                let ty = &s.path;
                Ok(self.emit(
                    quote!(::bitlogic::__private::Symbolic::structure::<#ty>(vec![#(#fields),*])?),
                ))
            }
            Expr::Lit(lit) if matches!(lit.lit, syn::Lit::Bool(_)) => Ok(self
                .emit(quote!(::bitlogic::__private::Symbolic::constant(__logic_builder, #lit)?))),
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Not(_)) => {
                let a = self.expr(&u.expr, env)?;
                Ok(self.emit(quote!(#a.not(__logic_builder)?)))
            }
            Expr::Binary(b) => {
                let a = self.expr(&b.left, env)?;
                if matches!(b.op, syn::BinOp::Shl(_) | syn::BinOp::Shr(_)) {
                    let amount = u32::try_from(integer(&b.right)?)
                        .map_err(|_| error(b, "shift literal exceeds u32"))?;
                    let left = matches!(b.op, syn::BinOp::Shl(_));
                    return Ok(self.emit(quote!(#a.shift(__logic_builder,#amount,#left)?)));
                }
                let rhs = self.expr(&b.right, env)?;
                let op = match b.op {
                    syn::BinOp::BitAnd(_) | syn::BinOp::And(_) => "And",
                    syn::BinOp::BitOr(_) | syn::BinOp::Or(_) => "Or",
                    syn::BinOp::BitXor(_) => "Xor",
                    syn::BinOp::Add(_) => "Add",
                    syn::BinOp::Sub(_) => "Sub",
                    syn::BinOp::Eq(_) => "Eq",
                    syn::BinOp::Ne(_) => "Ne",
                    syn::BinOp::Lt(_) => "Lt",
                    syn::BinOp::Le(_) => "Le",
                    syn::BinOp::Gt(_) => "Gt",
                    syn::BinOp::Ge(_) => "Ge",
                    _ => return Err(error(b, "unsupported binary operation or mutation")),
                };
                let op = format_ident!("{op}");
                let logical = matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_));
                Ok(self.emit(
                    quote!(#a.binary(__logic_builder,::bitlogic::Binary::#op,&#rhs,#logical)?),
                ))
            }
            Expr::Call(call) => {
                let Expr::Path(path) = &*call.func else {
                    return Err(error(
                        call,
                        "only bits::<N>(literal) and calls to #[logic] functions by path are supported",
                    ));
                };
                if path.qself.is_some() {
                    return Err(error(call, "call #[logic] functions by plain path"));
                }
                let last = path.path.segments.last().unwrap();
                if last.ident != "bits" {
                    return self.call(call, &path.path, env);
                }
                if call.args.len() != 1 {
                    return Err(error(call, "bits requires one integer literal"));
                }
                let generic = generic_integers(&last.arguments)?;
                if generic.len() != 1 || !(1..=64).contains(&generic[0]) {
                    return Err(error(call, "bits requires one width in 1..=64"));
                }
                let n = generic[0] as usize;
                let value = integer(&call.args[0])?;
                Ok(self.emit(quote!(::bitlogic::__private::Symbolic::constant(__logic_builder,::bitlogic::bits::<#n>(#value))?)))
            }
            Expr::If(branch) => {
                let c = self.expr(&branch.cond, env)?;
                let y = self.block(&branch.then_branch, env)?;
                let Some((_, otherwise)) = &branch.else_branch else {
                    return Err(error(branch, "value conditionals require else"));
                };
                let n = self.expr(otherwise, env)?;
                Ok(self.emit(
                    quote!(::bitlogic::__private::Symbolic::mux(__logic_builder,&#c,&#y,&#n)?),
                ))
            }
            Expr::MethodCall(call) => {
                let a = self.expr(&call.receiver, env)?;
                let generic = call
                    .turbofish
                    .as_ref()
                    .map(|g| generic_integers(&PathArguments::AngleBracketed(g.clone())))
                    .transpose()?
                    .unwrap_or_default();
                match call.method.to_string().as_str() {
                    "fold" if generic.len() == 1 && call.args.len() == 1 => {
                        let count = generic[0];
                        if count > 256 {
                            return Err(error(call, "fold is limited to 256 iterations"));
                        }
                        let Expr::Closure(closure) = &call.args[0] else {
                            return Err(error(call, "fold requires |acc, index| expression"));
                        };
                        if closure.inputs.len() != 2
                            || closure.asyncness.is_some()
                            || closure.movability.is_some()
                            || closure.capture.is_some()
                            || !matches!(closure.output, syn::ReturnType::Default)
                        {
                            return Err(error(closure, "fold requires |acc, index| expression"));
                        }
                        let acc = immutable_binding(&closure.inputs[0])?;
                        let index = immutable_binding(&closure.inputs[1])?;
                        if acc == index {
                            return Err(error(closure, "fold bindings must be distinct"));
                        }
                        // Validate even an unexecuted zero-iteration body against
                        // the pure language contract, without emitting its graph.
                        if count == 0 {
                            let mut validation = Lower {
                                work: self.work,
                                ..Lower::default()
                            };
                            let mut inner = env.clone();
                            inner.insert(clean(acc), a.clone());
                            inner.insert(clean(index), quote!(__unused_index));
                            validation.expr(&closure.body, &inner)?;
                            self.work = validation.work;
                        }
                        let mut value = a;
                        for i in 0..count {
                            let idx = self.emit(quote!(::bitlogic::__private::Symbolic::constant(__logic_builder,::bitlogic::bits::<8>(#i))?));
                            let mut inner = env.clone();
                            inner.insert(clean(acc), value.clone());
                            inner.insert(clean(index), idx);
                            let next = self.expr(&closure.body, &inner)?;
                            self.statements.push(quote!(if #value.schema != #next.schema { return Err(::bitlogic::Error("fold accumulator type mismatch".into())); }));
                            value = next;
                        }
                        Ok(value)
                    }
                    "bit" | "slice" if call.args.len() == 1 => {
                        let bit = call.method == "bit";
                        let width = if bit && generic.is_empty() {
                            1
                        } else if !bit && generic.len() == 1 {
                            generic[0]
                        } else {
                            return Err(error(call, "use bit(index) or slice::<N>(offset)"));
                        };
                        let width =
                            u8::try_from(width).map_err(|_| error(call, "slice out of bounds"))?;
                        let offset = u8::try_from(integer(&call.args[0])?)
                            .map_err(|_| error(call, "slice out of bounds"))?;
                        Ok(self.emit(quote!(#a.slice(__logic_builder,#offset,#width,#bit)?)))
                    }
                    "at" if call.args.len() == 1 && generic.is_empty() => {
                        let i = self.expr(&call.args[0], env)?;
                        Ok(self.emit(quote!(#a.at(__logic_builder,&#i)?)))
                    }
                    "set" if call.args.len() == 2 && generic.is_empty() => {
                        let i = self.expr(&call.args[0], env)?;
                        let v = self.expr(&call.args[1], env)?;
                        Ok(self.emit(quote!(#a.set(__logic_builder,&#i,&#v)?)))
                    }
                    "concat" if call.args.len() == 1 && generic.len() == 2 => {
                        let b = self.expr(&call.args[0], env)?;
                        let low = u8::try_from(generic[0])
                            .map_err(|_| error(call, "concatenation width mismatch"))?;
                        let total = u8::try_from(generic[1])
                            .map_err(|_| error(call, "concatenation width mismatch"))?;
                        Ok(self.emit(quote!(#a.concat(__logic_builder,&#b,#low,#total)?)))
                    }
                    _ => Err(error(
                        call,
                        "supported methods: bit, slice, concat, at, set, bounded fold",
                    )),
                }
            }
            _ => Err(error(
                expr,
                "unsupported pure logic expression (mutation, references, early returns and struct updates are forbidden)",
            )),
        }
    }
}
fn immutable_binding(pat: &Pat) -> syn::Result<&syn::Ident> {
    let Pat::Ident(binding) = pat else {
        return Err(error(pat, "use a named immutable binding"));
    };
    if binding.by_ref.is_some() || binding.mutability.is_some() || binding.subpat.is_some() {
        return Err(error(pat, "logic bindings must be immutable values"));
    }
    Ok(&binding.ident)
}
fn expand(function: ItemFn) -> syn::Result<Tokens> {
    let sig = &function.sig;
    if sig.asyncness.is_some()
        || sig.constness.is_some()
        || sig.unsafety.is_some()
        || sig.abi.is_some()
        || sig.variadic.is_some()
        || !sig.generics.params.is_empty()
        || sig.generics.where_clause.is_some()
    {
        return Err(error(
            sig,
            "logic functions must be ordinary non-generic functions with owned inputs and an explicit output type",
        ));
    }
    let syn::ReturnType::Type(_, output) = &sig.output else {
        return Err(error(
            sig,
            "logic functions require an explicit output type",
        ));
    };
    value_type(output)?;
    let mut lower = Lower::default();
    let mut env = Env::new();
    let mut inputs = Vec::new();
    for (index, arg) in sig.inputs.iter().enumerate() {
        let FnArg::Typed(arg) = arg else {
            return Err(error(sig, "logic functions cannot have receivers"));
        };
        value_type(&arg.ty)?;
        let name = clean(immutable_binding(&arg.pat)?);
        if env.contains_key(&name) {
            return Err(error(arg, "logic parameters must have distinct names"));
        }
        let ty = &arg.ty;
        lower
            .statements
            .push(quote!(__logic_args[#index].require::<#ty>()?;));
        env.insert(name.clone(), quote!(__logic_args[#index]));
        inputs.push((name, ty));
    }
    let result = lower.block(&function.block, &env)?;
    // Native bodies get `at`/`set` on arrays without an explicit import.
    let mut function = function.clone();
    function.block.stmts.insert(
        0,
        syn::parse_quote!(
            #[allow(unused_imports)]
            use ::bitlogic::LogicArray as _;
        ),
    );
    let statements = lower.statements;
    let arity = inputs.len();
    let name = &sig.ident;
    let vis = &function.vis;
    // Only single-input functions are complete circuits with an input schema;
    // other arities are helpers that callers inline.
    let ir = if let [(root, input)] = inputs.as_slice() {
        quote! {
            pub fn ir() -> ::std::result::Result<::bitlogic::Logic,::bitlogic::Error> {
                let mut __logic_builder = ::bitlogic::Builder::default();
                let input = ::bitlogic::__private::Symbolic::input::<#input>(&mut __logic_builder,#root)?;
                let result = Self::__lower(&mut __logic_builder, ::std::vec![input], 0)?;
                result.output(&mut __logic_builder)?;
                __logic_builder.finish()?.with_schemas(<#input as ::bitlogic::LogicValue>::schema(),<#output as ::bitlogic::LogicValue>::schema())
            }
        }
    } else {
        quote!()
    };
    Ok(quote! {
        #function
        #[allow(non_camel_case_types)]
        #vis struct #name {}
        impl #name {
            #ir
            /// Build this function's graph into `__logic_builder` from
            /// already-lowered arguments. Used by calling `#[logic]` functions.
            #[doc(hidden)]
            #[allow(unused_mut, unused_variables)]
            pub fn __lower(
                __logic_builder: &mut ::bitlogic::Builder,
                __logic_args: ::std::vec::Vec<::bitlogic::__private::Symbolic>,
                __logic_depth: usize,
            ) -> ::std::result::Result<::bitlogic::__private::Symbolic,::bitlogic::Error> {
                if __logic_depth > ::bitlogic::__private::MAX_CALL_DEPTH {
                    return Err(::bitlogic::Error("logic call depth exceeds 64 (recursive #[logic] call?)".into()));
                }
                if __logic_args.len() != #arity {
                    return Err(::bitlogic::Error("logic call argument count mismatch".into()));
                }
                let mut __logic_values: ::std::vec::Vec<::bitlogic::__private::Symbolic> = ::std::vec::Vec::new();
                #(#statements)*
                #result.require::<#output>()?;
                Ok(#result.clone())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_mutation_and_unsupported_syntax() {
        for (source, message) in [
            ("fn f(a:&bool)->bool {*a}", "owned"),
            ("fn f(mut a:bool)->bool {a}", "immutable"),
            ("fn f(a:bool)->bool {let mut x=a;x}", "immutable"),
            ("fn f(a:bool)->bool {a=false;a}", "immutable let"),
            ("fn f(a:bool)->bool {return a;}", "trailing"),
            ("fn f(a:bool)->bool {if a {true}}", "require else"),
            ("fn f(a:bool,a:bool)->bool {a}", "distinct"),
            ("fn f<T>(a:bool)->bool {a}", "non-generic"),
            ("fn f(a:bool)->bool {g::<1>(a)}", "non-generic"),
            ("fn f(a:bool)->bool {(g)(a)}", "by path"),
            ("fn f(a:bool) {a}", "explicit output"),
            (
                "fn f(a:bool)->bool {a.fold::<257>(|acc,i| acc)}",
                "256 iterations",
            ),
            (
                "fn f(a:bool)->bool {a.fold::<0>(|acc,i| {let mut x=acc;x})}",
                "immutable",
            ),
            (
                "fn f(a:bool)->bool {a.fold::<256>(|acc,i| acc.fold::<256>(|x,j| !x))}",
                "65536 lowering",
            ),
            ("fn f(a:bool)->bool {loop {}}", "unsupported"),
            ("fn f(a:[bool;2])->bool {a[a[0]]}", "literal"),
            (
                "fn f(a:bool)->bool {match a { true if a => a, _ => a }}",
                "guards",
            ),
            (
                "fn f(a:bool)->bool {match a { x @ true => a, _ => a }}",
                "patterns",
            ),
            ("fn f(a:bool)->bool {match a { a => a }}", "patterns"),
            ("fn f(a:bool)->[bool;2] {[a; N]}", "integer literal"),
            ("fn f(a:&[bool;2])->bool {a[0]}", "owned"),
        ] {
            let err = expand(syn::parse_str(source).unwrap()).unwrap_err();
            assert!(err.to_string().contains(message), "{source}: {err}");
        }
    }
    #[test]
    fn rejects_unsupported_enums() {
        for (source, message) in [
            ("enum E { A(bool) }", "fieldless"),
            ("enum E { A { x: bool } }", "fieldless"),
            ("enum E {}", "at least one variant"),
            ("enum E { A = 1, B = 1 }", "duplicate"),
            ("enum E { A = 1, B = 0, C }", "duplicate"),
            ("enum E { A = X }", "integer literal"),
        ] {
            let err = derive_value(syn::parse_str(source).unwrap()).unwrap_err();
            assert!(err.to_string().contains(message), "{source}: {err}");
        }
    }
}
