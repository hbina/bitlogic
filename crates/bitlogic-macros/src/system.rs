//! `system!`: units, memories and boundary registers declared once, expanded
//! into a typed Rust cycle model and a `Design` for the Verilog backend.
use crate::error;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use std::collections::BTreeMap;
use syn::{
    Attribute, Expr, ExprStruct, Ident, LitInt, Member, Token, Type, Visibility, braced,
    parenthesized,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

/// `[pub] mod name { item* }`
pub struct Spec {
    attrs: Vec<Attribute>,
    vis: Visibility,
    name: Ident,
    items: Vec<Item>,
}

enum Item {
    Param {
        attrs: Vec<Attribute>,
        name: Ident,
        ty: Type,
        default: Option<Expr>,
    },
    Input {
        attrs: Vec<Attribute>,
        name: Ident,
        ty: Type,
        registered: bool,
    },
    Output {
        attrs: Vec<Attribute>,
        name: Ident,
        ty: Type,
        source: Expr,
        registered: bool,
    },
    Memory {
        attrs: Vec<Attribute>,
        name: Ident,
        ty: Type,
        addr_bits: LitInt,
        contents: Option<Expr>,
    },
    Unit {
        attrs: Vec<Attribute>,
        name: Ident,
        state: Type,
        function: syn::Path,
        input: ExprStruct,
    },
    Write {
        memory: Ident,
        args: Vec<Expr>,
    },
    Read {
        memory: Ident,
        args: Vec<Expr>,
    },
    Port {
        attrs: Vec<Attribute>,
        name: Ident,
        req: Type,
        rsp: Type,
    },
    /// `request port(valid, data)`.
    Request {
        port: Ident,
        args: Vec<Expr>,
    },
    /// `respond port(ready)`.
    Respond {
        port: Ident,
        args: Vec<Expr>,
    },
}

fn keyword(input: ParseStream, word: &str) -> syn::Result<()> {
    let label: Ident = input.parse()?;
    if label != word {
        return Err(error(label, &format!("expected `{word}`")));
    }
    Ok(())
}

/// `, registered` or nothing.
fn registered(input: ParseStream) -> syn::Result<bool> {
    if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        keyword(input, "registered")?;
        return Ok(true);
    }
    Ok(false)
}

fn arguments(input: ParseStream) -> syn::Result<Vec<Expr>> {
    let content;
    parenthesized!(content in input);
    Ok(Punctuated::<Expr, Token![,]>::parse_terminated(&content)?
        .into_iter()
        .collect())
}

impl Parse for Item {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let kind: Ident = input.parse()?;
        let item = match kind.to_string().as_str() {
            "param" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let ty = input.parse()?;
                let default = if input.peek(Token![=]) {
                    input.parse::<Token![=]>()?;
                    Some(input.parse()?)
                } else {
                    None
                };
                Item::Param {
                    attrs,
                    name,
                    ty,
                    default,
                }
            }
            "input" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let ty = input.parse()?;
                Item::Input {
                    attrs,
                    name,
                    ty,
                    registered: registered(input)?,
                }
            }
            "output" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let ty = input.parse()?;
                input.parse::<Token![=]>()?;
                let source = input.parse()?;
                Item::Output {
                    attrs,
                    name,
                    ty,
                    source,
                    registered: registered(input)?,
                }
            }
            "ram" | "rom" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let ty = input.parse()?;
                input.parse::<Token![,]>()?;
                keyword(input, "addr_bits")?;
                input.parse::<Token![=]>()?;
                let addr_bits = input.parse()?;
                let contents = if kind == "rom" {
                    input.parse::<Token![,]>()?;
                    keyword(input, "contents")?;
                    input.parse::<Token![=]>()?;
                    Some(input.parse()?)
                } else {
                    None
                };
                Item::Memory {
                    attrs,
                    name,
                    ty,
                    addr_bits,
                    contents,
                }
            }
            "unit" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let state = input.parse()?;
                input.parse::<Token![=]>()?;
                let function = input.parse()?;
                let content;
                parenthesized!(content in input);
                let literal: ExprStruct = content.parse()?;
                if !content.is_empty() {
                    return Err(content.error("a unit takes one struct literal"));
                }
                Item::Unit {
                    attrs,
                    name,
                    state,
                    function,
                    input: literal,
                }
            }
            "write" | "read" | "request" | "respond" => {
                if let Some(attr) = attrs.first() {
                    return Err(error(attr, "port connections take no attributes"));
                }
                let target = input.parse()?;
                let args = arguments(input)?;
                match kind.to_string().as_str() {
                    "write" => Item::Write {
                        memory: target,
                        args,
                    },
                    "read" => Item::Read {
                        memory: target,
                        args,
                    },
                    "request" => Item::Request { port: target, args },
                    _ => Item::Respond { port: target, args },
                }
            }
            "port" => {
                let name = input.parse()?;
                input.parse::<Token![:]>()?;
                let req = input.parse()?;
                input.parse::<Token![=>]>()?;
                let rsp = input.parse()?;
                Item::Port {
                    attrs,
                    name,
                    req,
                    rsp,
                }
            }
            _ => {
                return Err(error(
                    kind,
                    "expected `param`, `input`, `output`, `ram`, `rom`, `unit`, `port`, \
                     `write`, `read`, `request` or `respond`",
                ));
            }
        };
        input.parse::<Token![;]>()?;
        Ok(item)
    }
}

impl Parse for Spec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let vis = input.parse()?;
        input.parse::<Token![mod]>()?;
        let name = input.parse()?;
        let content;
        braced!(content in input);
        let mut items = Vec::new();
        while !content.is_empty() {
            items.push(content.parse()?);
        }
        Ok(Spec {
            attrs,
            vis,
            name,
            items,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Param,
    Input { registered: bool },
    Output,
    Memory,
    Unit,
    Port,
}

#[derive(Clone, Copy, PartialEq)]
enum PortSignal {
    ReqReady,
    RspValid,
    Rsp,
}

/// Where a connection expression points.
enum Source {
    Param(Vec<Ident>),
    Input(Vec<Ident>),
    State(Ident, Vec<Ident>),
    Output(Ident, Vec<Ident>),
    Rdata(Ident),
    Port(Ident, PortSignal, Vec<Ident>),
}

/// `a.b.c` as identifiers.
fn path_of(expr: &Expr) -> syn::Result<Vec<Ident>> {
    match expr {
        Expr::Path(p) if p.qself.is_none() && p.attrs.is_empty() => match p.path.get_ident() {
            Some(id) => Ok(vec![id.clone()]),
            None => Err(error(p, "expected a name, not a path")),
        },
        Expr::Field(f) => {
            let mut path = path_of(&f.base)?;
            match &f.member {
                Member::Named(id) => path.push(id.clone()),
                Member::Unnamed(_) => return Err(error(f, "tuple fields are unsupported")),
            }
            Ok(path)
        }
        Expr::Paren(p) => path_of(&p.expr),
        _ => Err(error(
            expr,
            "connections are names: a param, an input, `unit.state.field`, \
             `unit.output`, `memory.rdata` or a port signal",
        )),
    }
}

fn names(path: &[Ident]) -> Vec<String> {
    path.iter().map(|i| i.to_string()).collect()
}

struct Context {
    kinds: BTreeMap<String, Kind>,
}

impl Context {
    fn source(&self, expr: &Expr) -> syn::Result<Source> {
        let path = path_of(expr)?;
        let (head, rest) = path.split_first().expect("non-empty");
        let Some(kind) = self.kinds.get(&head.to_string()) else {
            return Err(error(head, "unknown name in a connection"));
        };
        Ok(match kind {
            Kind::Param => Source::Param(path.clone()),
            Kind::Input { .. } => Source::Input(path.clone()),
            Kind::Output => {
                return Err(error(head, "outputs cannot drive connections"));
            }
            Kind::Memory => {
                if rest.len() != 1 || rest[0] != "rdata" {
                    return Err(error(expr, "a memory is read as `memory.rdata`"));
                }
                Source::Rdata(head.clone())
            }
            Kind::Port => match rest.split_first() {
                Some((s, [])) if s == "req_ready" => {
                    Source::Port(head.clone(), PortSignal::ReqReady, vec![])
                }
                Some((s, [])) if s == "rsp_valid" => {
                    Source::Port(head.clone(), PortSignal::RspValid, vec![])
                }
                Some((s, path)) if s == "rsp" => {
                    Source::Port(head.clone(), PortSignal::Rsp, path.to_vec())
                }
                _ => {
                    return Err(error(
                        expr,
                        "a port is read as `port.req_ready`, `port.rsp_valid` or `port.rsp`",
                    ));
                }
            },
            Kind::Unit => match rest.split_first() {
                Some((s, path)) if s == "state" => Source::State(head.clone(), path.to_vec()),
                Some(_) => Source::Output(head.clone(), rest.to_vec()),
                None => {
                    return Err(error(
                        expr,
                        "name a unit's `state.field` or one of its outputs",
                    ));
                }
            },
        })
    }

    /// The Rust expression for `source` inside `clock`, before the edge.
    fn native(&self, source: &Source) -> Tokens {
        match source {
            Source::Param(path) => quote!(self.#(#path).*),
            Source::Input(path) => {
                let registered = matches!(
                    self.kinds.get(&path[0].to_string()),
                    Some(Kind::Input { registered: true })
                );
                if registered {
                    quote!(self.#(#path).*)
                } else {
                    quote!(inputs.#(#path).*)
                }
            }
            Source::State(unit, path) => quote!(self.#unit #(.#path)*),
            Source::Output(unit, path) => {
                let local = format_ident!("__unit_{}", unit);
                quote!(#local.#(#path).*)
            }
            Source::Rdata(memory) => quote!(self.#memory.rdata),
            Source::Port(port, signal, path) => {
                let local = match signal {
                    PortSignal::ReqReady => format_ident!("__port_{}_ready", port),
                    PortSignal::RspValid => format_ident!("__port_{}_rsp_valid", port),
                    PortSignal::Rsp => format_ident!("__port_{}_rsp", port),
                };
                quote!(#local #(.#path)*)
            }
        }
    }

    /// The `bitlogic::system::Source` for `source`.
    fn design(&self, source: &Source) -> Tokens {
        let path = |p: &[Ident]| {
            let p = names(p);
            quote!(::std::vec![#(#p),*])
        };
        match source {
            Source::Param(p) => {
                let (name, rest) = (p[0].to_string(), path(&p[1..]));
                quote!(::bitlogic::system::Source::Param { name: #name, path: #rest })
            }
            Source::Input(p) => {
                let (name, rest) = (p[0].to_string(), path(&p[1..]));
                quote!(::bitlogic::system::Source::Input { name: #name, path: #rest })
            }
            Source::State(u, p) => {
                let (unit, rest) = (u.to_string(), path(p));
                quote!(::bitlogic::system::Source::State { unit: #unit, path: #rest })
            }
            Source::Output(u, p) => {
                let (unit, rest) = (u.to_string(), path(p));
                quote!(::bitlogic::system::Source::Output { unit: #unit, path: #rest })
            }
            Source::Rdata(m) => {
                let memory = m.to_string();
                quote!(::bitlogic::system::Source::Rdata { memory: #memory })
            }
            Source::Port(port, signal, p) => {
                let (name, rest) = (port.to_string(), path(p));
                let signal = match signal {
                    PortSignal::ReqReady => quote!(ReqReady),
                    PortSignal::RspValid => quote!(RspValid),
                    PortSignal::Rsp => quote!(Rsp),
                };
                quote!(::bitlogic::system::Source::Port {
                    name: #name,
                    signal: ::bitlogic::system::PortSignal::#signal,
                    path: #rest,
                })
            }
        }
    }
}

const RESERVED: &[&str] = &[
    "eval", "state", "inputs", "clk", "rst", "new", "clock", "reset", "edge", "outputs",
];

pub fn expand(spec: Spec) -> syn::Result<Tokens> {
    let Spec {
        attrs,
        vis,
        name,
        items,
    } = spec;
    // Every top-level name and its kind.
    let mut kinds = BTreeMap::new();
    let mut memories = BTreeMap::new();
    for item in &items {
        let (id, kind) = match item {
            Item::Param { name, .. } => (name, Kind::Param),
            Item::Input {
                name, registered, ..
            } => (
                name,
                Kind::Input {
                    registered: *registered,
                },
            ),
            Item::Output { name, .. } => (name, Kind::Output),
            Item::Memory { name, contents, .. } => {
                memories.insert(name.to_string(), contents.is_some());
                (name, Kind::Memory)
            }
            Item::Unit { name, .. } => (name, Kind::Unit),
            Item::Port { name, .. } => (name, Kind::Port),
            Item::Write { .. }
            | Item::Read { .. }
            | Item::Request { .. }
            | Item::Respond { .. } => continue,
        };
        let key = id.to_string();
        if RESERVED.contains(&key.as_str()) || key.starts_with("__") {
            return Err(error(id, "this name is reserved in a system"));
        }
        if kinds.insert(key, kind).is_some() {
            return Err(error(id, "duplicate name in a system"));
        }
    }
    let cx = Context { kinds };

    let mut fields = Vec::new();
    let mut inits = Vec::new();
    let mut new_params = Vec::new();
    let mut input_fields = Vec::new();
    let mut input_zero = Vec::new();
    let mut output_fields = Vec::new();
    let mut output_values = Vec::new();
    let mut evaluations = Vec::new();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut latches = Vec::new();
    let mut commits = Vec::new();
    let mut design_params = Vec::new();
    let mut design_inputs = Vec::new();
    let mut design_outputs = Vec::new();
    let mut design_memories = Vec::new();
    let mut design_units = Vec::new();
    let mut design_writes = Vec::new();
    let mut design_reads = Vec::new();
    let mut ports: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    // Per `port`: its types, then its `request` and `respond` connections.
    let mut links: BTreeMap<String, (Ident, Type, Type)> = BTreeMap::new();
    let mut requests: BTreeMap<String, (Source, Source)> = BTreeMap::new();
    let mut responds: BTreeMap<String, Source> = BTreeMap::new();
    let mut port_prelude = Vec::new();
    let mut port_cycles = Vec::new();

    for item in &items {
        match item {
            Item::Param {
                attrs,
                name,
                ty,
                default,
            } => {
                fields.push(quote!(#(#attrs)* pub #name: #ty,));
                new_params.push(quote!(#name: impl ::std::convert::Into<#ty>));
                inits.push(quote!(#name: #name.into(),));
                let label = name.to_string();
                let default = match default {
                    Some(e) => quote!({ let __v: #ty = #e; ::bitlogic::LogicValue::flatten(&__v) }),
                    None => quote!(<#ty as ::bitlogic::LogicValue>::schema().zero_values()),
                };
                design_params.push(quote!(::bitlogic::system::ParamSpec {
                    name: #label,
                    schema: <#ty as ::bitlogic::LogicValue>::schema(),
                    default: #default,
                }));
            }
            Item::Input {
                attrs,
                name,
                ty,
                registered,
            } => {
                input_fields.push(quote!(#(#attrs)* pub #name: #ty,));
                input_zero.push(quote!(#name: ::bitlogic::LogicValue::zeroed(),));
                if *registered {
                    fields.push(quote!(
                        /// The boundary register on this input.
                        pub #name: #ty,
                    ));
                    inits.push(quote!(#name: ::bitlogic::LogicValue::zeroed(),));
                    commits.push(quote!(self.#name = __zero_or(rst, inputs.#name);));
                }
                let label = name.to_string();
                design_inputs.push(quote!(::bitlogic::system::InputSpec {
                    name: #label,
                    schema: <#ty as ::bitlogic::LogicValue>::schema(),
                    registered: #registered,
                }));
            }
            Item::Output {
                attrs,
                name,
                ty,
                source,
                registered,
            } => {
                let src = cx.source(source)?;
                match (&src, registered) {
                    (Source::Output(..), false) => {
                        return Err(error(
                            source,
                            "a unit's combinational output must be `registered`",
                        ));
                    }
                    (Source::Input(p), false)
                        if cx.kinds[&p[0].to_string()] == (Kind::Input { registered: false }) =>
                    {
                        return Err(error(
                            source,
                            "an unregistered input cannot drive an output",
                        ));
                    }
                    (Source::Port(..), false) => {
                        return Err(error(
                            source,
                            "a port signal can drive an output only if it is `registered`",
                        ));
                    }
                    _ => {}
                }
                output_fields.push(quote!(#(#attrs)* pub #name: #ty,));
                if *registered {
                    let reg = format_ident!("{}", name);
                    fields.push(quote!(
                        /// The register on this output.
                        pub #reg: #ty,
                    ));
                    inits.push(quote!(#reg: ::bitlogic::LogicValue::zeroed(),));
                    let value = cx.native(&src);
                    let next = format_ident!("__next_{}", name);
                    latches.push(quote!(let #next: #ty = #value;));
                    commits.push(quote!(self.#reg = __zero_or(rst, #next);));
                    output_values.push(quote!(#name: self.#reg,));
                } else {
                    // Sampled after the edge from registered values.
                    let value = cx.native(&src);
                    output_values.push(quote!(#name: #value,));
                }
                let label = name.to_string();
                let design = cx.design(&src);
                design_outputs.push(quote!(::bitlogic::system::OutputSpec {
                    name: #label,
                    schema: <#ty as ::bitlogic::LogicValue>::schema(),
                    source: #design,
                    registered: #registered,
                }));
            }
            Item::Memory {
                attrs,
                name,
                ty,
                addr_bits,
                contents,
            } => {
                let bits: u32 = addr_bits.base10_parse()?;
                if !(1..=24).contains(&bits) {
                    return Err(error(addr_bits, "addr_bits must be in 1..=24"));
                }
                fields.push(quote!(#(#attrs)* pub #name: ::bitlogic::Memory<#ty>,));
                let make = match contents {
                    Some(c) => quote!(::bitlogic::Memory::<#ty>::rom(#bits, #c)),
                    None => quote!(::bitlogic::Memory::<#ty>::new(#bits)),
                };
                inits.push(quote!(#name: #make,));
                let label = name.to_string();
                let contents = match contents {
                    Some(c) => quote!(::std::option::Option::Some(
                        ::bitlogic::Memory::<#ty>::rom(#bits, #c)
                            .cells
                            .iter()
                            .flat_map(::bitlogic::LogicValue::flatten)
                            .collect()
                    )),
                    None => quote!(::std::option::Option::None),
                };
                design_memories.push(quote!(::bitlogic::system::MemorySpec {
                    name: #label,
                    element: <#ty as ::bitlogic::LogicValue>::schema(),
                    addr_bits: #bits,
                    contents: #contents,
                }));
            }
            Item::Unit {
                attrs,
                name,
                state,
                function,
                input,
            } => {
                if input.rest.is_some() || input.dot2_token.is_some() {
                    return Err(error(
                        input,
                        "unit inputs list every field; `..` is unsupported",
                    ));
                }
                fields.push(quote!(#(#attrs)* pub #name: #state,));
                inits.push(quote!(#name: ::bitlogic::LogicValue::zeroed(),));
                let mut values = Vec::new();
                let mut connections = Vec::new();
                let mut has_state = false;
                for field in &input.fields {
                    let Member::Named(member) = &field.member else {
                        return Err(error(field, "unit inputs have named fields"));
                    };
                    if member == "state" {
                        let ok =
                            path_of(&field.expr).is_ok_and(|p| p.len() == 1 && p[0] == "state");
                        if !ok {
                            return Err(error(&field.expr, "the `state` field must be `state`"));
                        }
                        has_state = true;
                        values.push(quote!(state: self.#name,));
                        continue;
                    }
                    let src = cx.source(&field.expr)?;
                    if let Source::Output(..) = src {
                        return Err(error(
                            &field.expr,
                            "a unit's combinational outputs may drive only memory ports and \
                             outputs; use `unit.state.field` for a registered value",
                        ));
                    }
                    let value = cx.native(&src);
                    values.push(quote!(#member: #value,));
                    let label = member.to_string();
                    let design = cx.design(&src);
                    connections.push(quote!((#label, #design)));
                }
                if !has_state {
                    return Err(error(input, "a unit input needs `state`"));
                }
                let local = format_ident!("__unit_{}", name);
                let ty = &input.path;
                evaluations.push(quote!(
                    let #local = self.eval.eval::<#function>(#ty { #(#values)* });
                ));
                commits.push(quote!(self.#name = __zero_or(rst, #local.state);));
                let label = name.to_string();
                design_units.push(quote!(::bitlogic::system::UnitSpec {
                    name: #label,
                    logic: <#function as ::bitlogic::LogicFn>::graph()?,
                    state: <#state as ::bitlogic::LogicValue>::schema(),
                    connections: ::std::vec![#(#connections),*],
                }));
            }
            Item::Port {
                attrs,
                name,
                req,
                rsp,
            } => {
                fields.push(quote!(
                    #(#attrs)*
                    /// The other side of this port: replace it to connect one.
                    pub #name: ::std::boxed::Box<dyn ::bitlogic::system::Responder<#req, #rsp>>,
                ));
                inits.push(quote!(#name: ::std::boxed::Box::new(::bitlogic::system::Unconnected),));
                let last = format_ident!("__port_{}", name);
                fields.push(quote!(#last: ::bitlogic::system::PortCycle<#req, #rsp>,));
                inits.push(quote!(#last: ::bitlogic::system::PortCycle::idle(),));
                output_fields.push(quote!(
                    /// This port's signals during the cycle before the edge.
                    pub #name: ::bitlogic::system::PortCycle<#req, #rsp>,
                ));
                output_values.push(quote!(#name: self.#last,));
                links.insert(name.to_string(), (name.clone(), req.clone(), rsp.clone()));
            }
            Item::Request { port, args } | Item::Respond { port, args } => {
                let request = matches!(item, Item::Request { .. });
                if cx.kinds.get(&port.to_string()) != Some(&Kind::Port) {
                    return Err(error(port, "not a declared `port`"));
                }
                let want = if request { 2 } else { 1 };
                if args.len() != want {
                    let shape = if request {
                        "request port(valid, data)"
                    } else {
                        "respond port(ready)"
                    };
                    return Err(error(port, &format!("expected `{shape}`")));
                }
                let srcs = args
                    .iter()
                    .map(|a| cx.source(a))
                    .collect::<syn::Result<Vec<_>>>()?;
                for (src, arg) in srcs.iter().zip(args) {
                    if let Source::Port(..) = src {
                        return Err(error(arg, "a port signal cannot drive a port"));
                    }
                }
                let key = port.to_string();
                let mut srcs = srcs.into_iter();
                let duplicate = if request {
                    let valid = srcs.next().expect("two");
                    let data = srcs.next().expect("two");
                    requests.insert(key, (valid, data)).is_some()
                } else {
                    responds.insert(key, srcs.next().expect("one")).is_some()
                };
                if duplicate {
                    return Err(error(port, "this port is already connected"));
                }
            }
            Item::Write { memory, args } | Item::Read { memory, args } => {
                let write = matches!(item, Item::Write { .. });
                let Some(&rom) = memories.get(&memory.to_string()) else {
                    return Err(error(memory, "not a declared `ram` or `rom`"));
                };
                if write && rom {
                    return Err(error(memory, "a ROM has no write port"));
                }
                let want = if write { 3 } else { 1 };
                if args.len() != want {
                    let shape = if write {
                        "write mem(enable, address, data)"
                    } else {
                        "read mem(address)"
                    };
                    return Err(error(memory, &format!("expected `{shape}`")));
                }
                let entry = ports.entry(memory.to_string()).or_default();
                let count = if write { &mut entry.0 } else { &mut entry.1 };
                *count += 1;
                if *count > 1 {
                    return Err(error(memory, "this port is already connected"));
                }
                let srcs = args
                    .iter()
                    .map(|a| cx.source(a))
                    .collect::<syn::Result<Vec<_>>>()?;
                let native: Vec<_> = srcs.iter().map(|s| cx.native(s)).collect();
                let design: Vec<_> = srcs.iter().map(|s| cx.design(s)).collect();
                let label = memory.to_string();
                if write {
                    let (en, addr, data) = (&native[0], &native[1], &native[2]);
                    writes.push(quote!(if #en { self.#memory.write(#addr, #data); }));
                    let (en, addr, data) = (&design[0], &design[1], &design[2]);
                    design_writes.push(quote!(::bitlogic::system::WriteSpec {
                        memory: #label, enable: #en, address: #addr, data: #data,
                    }));
                } else {
                    let local = format_ident!("__rdata_{}", memory);
                    let addr = &native[0];
                    reads.push(quote!(let #local = self.#memory.read(#addr);));
                    commits.push(quote!(self.#memory.rdata = #local;));
                    let addr = &design[0];
                    design_reads.push(quote!(::bitlogic::system::ReadSpec {
                        memory: #label, address: #addr,
                    }));
                }
            }
        }
    }
    let mut design_ports = Vec::new();
    for (key, (port, req, rsp)) in &links {
        let (Some((valid, data)), Some(ready)) = (requests.get(key), responds.get(key)) else {
            return Err(error(port, "this port needs `request` and `respond`"));
        };
        let ready_local = format_ident!("__port_{}_ready", port);
        let offer = format_ident!("__port_{}_offer", port);
        let rsp_valid = format_ident!("__port_{}_rsp_valid", port);
        let rsp_local = format_ident!("__port_{}_rsp", port);
        port_prelude.push(quote!(
            let #ready_local: bool = self.#port.req_ready();
            let #offer: ::std::option::Option<#rsp> = self.#port.rsp();
            let #rsp_valid: bool = #offer.is_some();
            let #rsp_local: #rsp = #offer.unwrap_or_else(::bitlogic::LogicValue::zeroed);
        ));
        let cycle = format_ident!("__cycle_{}", port);
        let last = format_ident!("__port_{}", port);
        let (v, d, r) = (cx.native(valid), cx.native(data), cx.native(ready));
        port_cycles.push(quote!(
            let #cycle: ::bitlogic::system::PortCycle<#req, #rsp> = ::bitlogic::system::PortCycle {
                req_valid: #v,
                req: #d,
                req_ready: #ready_local,
                rsp_valid: #rsp_valid,
                rsp: #rsp_local,
                rsp_ready: #r,
            };
        ));
        commits.push(quote!(
            self.#port.edge(#cycle.request(), #cycle.response().is_some());
            self.#last = #cycle;
        ));
        let label = port.to_string();
        let (v, d, r) = (cx.design(valid), cx.design(data), cx.design(ready));
        design_ports.push(quote!(::bitlogic::system::PortSpec {
            name: #label,
            request: <#req as ::bitlogic::LogicValue>::schema(),
            response: <#rsp as ::bitlogic::LogicValue>::schema(),
            req_valid: #v,
            req: #d,
            rsp_ready: #r,
        }));
    }
    for (memory, rom) in &memories {
        let (w, r) = ports.get(memory).copied().unwrap_or_default();
        if r != 1 || w != usize::from(!rom) {
            let id = items
                .iter()
                .find_map(|i| match i {
                    Item::Memory { name, .. } if name == memory => Some(name),
                    _ => None,
                })
                .expect("declared");
            let need = if *rom {
                "a `read` port"
            } else {
                "one `write` and one `read` port"
            };
            return Err(error(id, &format!("this memory needs {need}")));
        }
    }

    let label = name.to_string();
    let doc = format!(
        "Cycle model of the `{label}` system. `clock` advances one edge and \
         `reset` one edge with the synchronous reset asserted; all state and \
         boundary registers start at zero (the reset state)."
    );
    Ok(quote! {
        #(#attrs)*
        #vis mod #name {
            #![allow(unused_imports, dead_code, clippy::all)]
            use super::*;

            /// Values on the system's input pins for one cycle.
            #[derive(Clone, Copy, Debug, PartialEq)]
            pub struct Inputs { #(#input_fields)* }

            /// Output pins just after an edge.
            #[derive(Clone, Copy, Debug, PartialEq)]
            pub struct Outputs { #(#output_fields)* }

            #[doc = #doc]
            pub struct Core<E: ::bitlogic::Evaluate = ::bitlogic::Native> {
                pub eval: E,
                #(#fields)*
            }

            impl<E: ::bitlogic::Evaluate> Core<E> {
                /// The system after two reset edges with every input zero,
                /// so each memory's read data matches its reset-state address.
                pub fn new(eval: E, #(#new_params),*) -> Self {
                    let mut core = Self { eval, #(#inits)* };
                    let zero = Inputs { #(#input_zero)* };
                    core.reset(zero);
                    core.reset(zero);
                    core
                }

                /// One clock edge with `inputs` on the pins; returns the
                /// outputs just after the edge.
                pub fn clock(&mut self, inputs: impl ::std::convert::Into<Inputs>) -> Outputs {
                    self.edge(inputs.into(), false)
                }

                /// One clock edge with the synchronous reset asserted: every
                /// state and boundary register becomes zero. Memories keep
                /// their contents and their ports work as on any edge.
                pub fn reset(&mut self, inputs: impl ::std::convert::Into<Inputs>) -> Outputs {
                    self.edge(inputs.into(), true)
                }

                fn edge(&mut self, inputs: Inputs, rst: bool) -> Outputs {
                    fn __zero_or<T: ::bitlogic::LogicValue>(rst: bool, next: T) -> T {
                        if rst { T::zeroed() } else { next }
                    }
                    // What each port's other side offers this cycle.
                    #(#port_prelude)*
                    // Units see only registered values and port signals, so
                    // order is free.
                    #(#evaluations)*
                    // Read-first memories: sample before this edge's writes.
                    #(#reads)*
                    #(#latches)*
                    #(#port_cycles)*
                    #(#writes)*
                    #(#commits)*
                    self.outputs()
                }

                /// The output pins as they stand.
                pub fn outputs(&self) -> Outputs {
                    Outputs { #(#output_values)* }
                }
            }

            /// This system as data for the Verilog backend.
            pub fn design() -> ::std::result::Result<::bitlogic::system::Design, ::bitlogic::Error> {
                ::std::result::Result::Ok(::bitlogic::system::Design {
                    name: #label,
                    params: ::std::vec![#(#design_params),*],
                    inputs: ::std::vec![#(#design_inputs),*],
                    outputs: ::std::vec![#(#design_outputs),*],
                    memories: ::std::vec![#(#design_memories),*],
                    units: ::std::vec![#(#design_units),*],
                    writes: ::std::vec![#(#design_writes),*],
                    reads: ::std::vec![#(#design_reads),*],
                    ports: ::std::vec![#(#design_ports),*],
                })
            }

            /// Every generated Verilog file as `(file name, contents)`.
            pub fn verilog() -> ::std::result::Result<::std::vec::Vec<(::std::string::String, ::std::string::String)>, ::bitlogic::Error> {
                design()?.verilog()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "param cfg: Cfg; input go: bool; input sync: bool, registered; \
                        ram buf: Bits<8>, addr_bits = 4; \
                        rom table: Bits<8>, addr_bits = 2, contents = T;";

    fn check(items: &str) -> syn::Result<Tokens> {
        expand(syn::parse_str(&format!("mod s {{ {BASE} {items} }}"))?)
    }

    #[test]
    fn accepts_a_complete_system() {
        check(
            "unit u: S = f(I { state, go, sync, cfg, a: u.state.x, b: buf.rdata, c: table.rdata }); \
             write buf(u.we, u.addr, u.data); read buf(u.addr); read table(u.state.x); \
             output o: bool = u.we, registered; output p: bool = u.state.y; \
             output q: bool = sync; output r: Bits<8> = buf.rdata;",
        )
        .unwrap();
    }

    #[test]
    fn rejects_bad_connections() {
        let unit = "unit u: S = f(I { state, a: go });";
        let ports = "write buf(u.we, u.addr, u.data); read buf(u.addr); read table(u.addr);";
        for (items, message) in [
            (
                "unit u: S = f(I { state, a: u.we }); write buf(u.we, u.addr, u.data); \
                 read buf(u.addr); read table(u.addr);",
                "may drive only memory ports",
            ),
            (
                &format!("{unit} {ports} output o: bool = u.we;"),
                "must be `registered`",
            ),
            (
                &format!("{unit} {ports} output o: bool = go;"),
                "unregistered input",
            ),
            (
                &format!("{unit} write buf(u.we, u.addr, u.data); read table(u.addr);"),
                "needs one `write`",
            ),
            (
                &format!("{unit} {ports} read buf(u.addr);"),
                "already connected",
            ),
            (
                &format!("{unit} {ports} write table(u.we, u.addr, u.data);"),
                "no write port",
            ),
            (
                &format!("{unit} {ports} read nothing(u.addr);"),
                "not a declared",
            ),
            (
                &format!("{unit} {ports} write buf(u.we, u.addr);"),
                "expected `write",
            ),
            ("unit u: S = f(I { a: go });", "needs `state`"),
            ("unit u: S = f(I { state: go });", "must be `state`"),
            ("unit u: S = f(I { state, ..x });", "`..` is unsupported"),
            ("unit u: S = f(I { state, a: missing });", "unknown name"),
            ("unit u: S = f(I { state, a: buf.cells });", "memory.rdata"),
            ("unit u: S = f(I { state, a: u });", "state.field"),
            (
                "unit u: S = f(I { state, a: go + go });",
                "connections are names",
            ),
            ("unit go: S = f(I { state });", "duplicate name"),
            ("unit eval: S = f(I { state });", "reserved"),
            ("ram big: Bits<8>, addr_bits = 25;", "1..=24"),
            ("wire w: bool;", "expected `param`"),
            ("output o: bool = sync, latched;", "expected `registered`"),
        ] {
            let err = check(items).unwrap_err().to_string();
            assert!(err.contains(message), "{items}: {err}");
        }
    }

    #[test]
    fn accepts_and_checks_ports() {
        let ports = "write buf(u.we, u.addr, u.data); read buf(u.addr); read table(u.addr);";
        let unit = "unit u: S = f(I { state, r: p.req_ready, v: p.rsp_valid, d: p.rsp.data });";
        let link = "request p(u.valid, u.req); respond p(u.ready);";
        check(&format!(
            "port p: Req => Rsp; {unit} {ports} {link} output s: bool = p.rsp_valid, registered;"
        ))
        .unwrap();
        for (items, message) in [
            (
                format!("port p: Req => Rsp; {unit} {ports} request p(u.valid, u.req);"),
                "needs `request` and `respond`",
            ),
            (
                format!("port p: Req => Rsp; {unit} {ports} respond p(u.ready);"),
                "needs `request` and `respond`",
            ),
            (
                format!("port p: Req => Rsp; {unit} {ports} {link} request p(u.valid, u.req);"),
                "already connected",
            ),
            (
                format!(
                    "port p: Req => Rsp; {unit} {ports} request p(u.valid); respond p(u.ready);"
                ),
                "expected `request",
            ),
            (
                format!(
                    "port p: Req => Rsp; {unit} {ports} request p(u.valid, u.req); respond p(u.ready, u.x);"
                ),
                "expected `respond",
            ),
            (
                format!(
                    "port p: Req => Rsp; {unit} {ports} request p(p.rsp_valid, u.req); respond p(u.ready);"
                ),
                "cannot drive a port",
            ),
            (
                format!("port p: Req => Rsp; {unit} {ports} {link} request q(u.valid, u.req);"),
                "not a declared `port`",
            ),
            (
                format!("port p: Req => Rsp; {unit} {ports} {link} output s: bool = p.rsp_valid;"),
                "only if it is `registered`",
            ),
            (
                format!(
                    "port p: Req => Rsp; unit u: S = f(I {{ state, r: p.req }}); {ports} {link}"
                ),
                "a port is read as",
            ),
            (
                format!("port p: Req => Rsp; unit u: S = f(I {{ state, r: p }}); {ports} {link}"),
                "a port is read as",
            ),
            (
                format!("port p: Req => Rsp; port p: Req => Rsp; {unit} {ports} {link}"),
                "duplicate name",
            ),
            ("port p: Req, Rsp;".to_string(), "expected `=>`"),
        ] {
            let err = check(&items).unwrap_err().to_string();
            assert!(err.contains(message), "{items}: {err}");
        }
    }
}
