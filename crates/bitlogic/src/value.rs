//! Structured values and the symbolic values used by generated frontend code.
use crate::{Binary, BitVec, Bits, Builder, Error, MAX_WIDTH, NodeId, Storage, Width, bits};
use std::any::{TypeId, type_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schema {
    Bool,
    Bits(u16),
    Struct {
        identity: TypeId,
        name: &'static str,
        fields: Vec<(String, Schema)>,
    },
    /// A fixed-length array; element `i` has leaf path `<path>.<i>`.
    Array {
        element: Box<Schema>,
        len: usize,
    },
    /// A fieldless enum stored as its discriminant in `width` bits. The first
    /// variant is the enum's zero value.
    Enum {
        identity: TypeId,
        name: &'static str,
        width: u16,
        variants: Vec<(String, u64)>,
    },
    /// An enum whose variants carry fields, stored as a tagged union: the
    /// `tag` leaf (a [`Schema::Enum`] of every variant), then a `payload`
    /// leaf of `Bits<P>`, `P` the widest variant's total width (no payload
    /// leaf when `P == 0`). The active variant's field leaves are packed into
    /// the payload from bit 0 upwards in declaration order; the bits above
    /// them are don't-care. Tuple fields are named `0`, `1`, ...
    Tagged {
        identity: TypeId,
        name: &'static str,
        tag: Box<Schema>,
        payloads: Vec<(String, Fields)>,
    },
}

/// A `match` arm's tests: each hit and the constant it compares against.
type Tests = Vec<(Symbolic, Option<BitVec>)>;

/// Named field schemas of a struct-like enum variant.
pub type Fields = Vec<(String, Schema)>;

impl Schema {
    pub fn structure<T: 'static>(fields: Vec<(String, Schema)>) -> Self {
        Self::Struct {
            identity: TypeId::of::<T>(),
            name: type_name::<T>(),
            fields,
        }
    }

    pub fn array<T: LogicValue>(len: usize) -> Self {
        Self::Array {
            element: Box::new(T::schema()),
            len,
        }
    }

    /// Schema of a fieldless enum with the given `(variant, discriminant)`s.
    pub fn enumeration<T: 'static>(variants: Vec<(String, u64)>) -> Self {
        let largest = variants.iter().map(|(_, v)| *v).max().unwrap_or(0);
        let width = (64 - largest.leading_zeros()).max(1) as u16;
        Self::Enum {
            identity: TypeId::of::<T>(),
            name: type_name::<T>(),
            width,
            variants,
        }
    }

    /// Schema of an enum whose variants carry fields, from
    /// `(variant, discriminant, fields)`.
    pub fn tagged<T: 'static>(variants: Vec<(String, u64, Fields)>) -> Self {
        let tag = Self::enumeration::<T>(
            variants
                .iter()
                .map(|(name, value, _)| (name.clone(), *value))
                .collect(),
        );
        Self::Tagged {
            identity: TypeId::of::<T>(),
            name: type_name::<T>(),
            tag: Box::new(tag),
            payloads: variants
                .into_iter()
                .map(|(name, _, fields)| (name, fields))
                .collect(),
        }
    }

    /// For a data enum: the variant's index, discriminant and fields.
    fn variant_fields(&self, variant: &str) -> Result<(usize, u64, &Fields), Error> {
        let Self::Tagged { tag, payloads, .. } = self else {
            return Err(Error("expected an enum with fields".into()));
        };
        let Self::Enum { variants, .. } = &**tag else {
            return Err(Error("malformed enum tag".into()));
        };
        payloads
            .iter()
            .zip(variants)
            .enumerate()
            .find(|(_, ((name, _), _))| name == variant)
            .map(|(index, ((_, fields), (_, value)))| (index, *value, fields))
            .ok_or_else(|| Error(format!("unknown enum variant {variant}")))
    }

    /// The scalar leaves of a data enum variant's fields: each leaf's path
    /// (`<field>` or `<field>.<subpath>`), bit offset in the payload and width.
    pub fn variant_layout(&self, variant: &str) -> Result<Vec<(String, u16, u16)>, Error> {
        let (_, _, fields) = self.variant_fields(variant)?;
        let mut offset = 0u32;
        let mut layout = Vec::new();
        for (name, field) in fields {
            for (path, leaf) in field.leaves() {
                let width = leaf.width()?;
                let path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{name}.{path}")
                };
                let at = u16::try_from(offset)
                    .ok()
                    .filter(|&at| u32::from(at) + u32::from(width) <= u32::from(MAX_WIDTH))
                    .ok_or_else(|| Error(format!("enum payload exceeds {MAX_WIDTH} bits")))?;
                layout.push((path, at, width));
                offset += u32::from(width);
            }
        }
        Ok(layout)
    }

    /// The payload width of a data enum: its widest variant's width, 0 when
    /// no variant has bits. Unchecked: a too wide payload makes an invalid
    /// `Bits` leaf.
    fn payload_bits(payloads: &[(String, Fields)]) -> u32 {
        payloads
            .iter()
            .map(|(_, fields)| {
                fields
                    .iter()
                    .flat_map(|(_, f)| f.leaves())
                    .map(|(_, leaf)| u32::from(leaf.width().unwrap_or(MAX_WIDTH)))
                    .sum::<u32>()
            })
            .max()
            .unwrap_or(0)
    }

    /// A Verilog comment for each leaf: an enum's encoding, or the layout of
    /// a data enum's payload (each variant's field leaves and their bits).
    pub(crate) fn leaf_notes(&self) -> Vec<Option<String>> {
        fn visit(schema: &Schema, out: &mut Vec<Option<String>>) {
            match schema {
                Schema::Struct { fields, .. } => {
                    for (_, field) in fields {
                        visit(field, out);
                    }
                }
                Schema::Array { element, len } => {
                    for _ in 0..*len {
                        visit(element, out);
                    }
                }
                Schema::Enum { name, variants, .. } => {
                    let variants: Vec<_> =
                        variants.iter().map(|(v, d)| format!("{v} = {d}")).collect();
                    out.push(Some(format!("{name} {{ {} }}", variants.join(", "))));
                }
                Schema::Tagged {
                    name,
                    tag,
                    payloads,
                    ..
                } => {
                    visit(tag, out);
                    if schema.payload_leaf().is_none() {
                        return;
                    }
                    let mut variants = Vec::new();
                    for (variant, _) in payloads {
                        let Ok(layout) = schema.variant_layout(variant) else {
                            continue;
                        };
                        let fields: Vec<_> = layout
                            .iter()
                            .map(|(path, at, width)| format!("{path} [{}:{at}]", at + width - 1))
                            .collect();
                        variants.push(if fields.is_empty() {
                            variant.clone()
                        } else {
                            format!("{variant} {{ {} }}", fields.join(", "))
                        });
                    }
                    out.push(Some(format!(
                        "{name} payload {{ {} }}",
                        variants.join(", ")
                    )));
                }
                _ => out.push(None),
            }
        }
        let mut out = Vec::new();
        visit(self, &mut out);
        out
    }

    /// The width of a data enum's payload leaf, if it has one. A too wide
    /// payload is `u16::MAX`, an invalid leaf width.
    fn payload_leaf(&self) -> Option<u16> {
        let Self::Tagged { payloads, .. } = self else {
            return None;
        };
        let width = Self::payload_bits(payloads);
        (width > 0).then(|| u16::try_from(width).unwrap_or(u16::MAX))
    }

    /// The leaves of a data enum value of `variant` with these field leaves
    /// (in [`Schema::leaves`] order of the variant's fields): the tag and the
    /// packed payload, its unused high bits zero.
    pub fn encode_variant(&self, variant: &str, fields: &[BitVec]) -> Result<Vec<BitVec>, Error> {
        let (index, _, _) = self.variant_fields(variant)?;
        Packing::new(self)?.encode(index, fields)
    }

    /// The active variant of a data enum value's leaves and that variant's
    /// field leaves, taken from the payload; payload bits above the variant
    /// are ignored.
    pub fn decode_variant(&self, values: &[BitVec]) -> Result<(String, Vec<BitVec>), Error> {
        let packing = Packing::new(self)?;
        let mut values = values.iter().cloned();
        let (index, fields) = packing.decode(&mut values)?;
        if values.next().is_some() {
            return Err(Error("too many value leaves".into()));
        }
        Ok((packing.variants[index].0.clone(), fields))
    }

    /// For each leaf of a value of this schema, the mask of its meaningful
    /// bits: all of them, except payload bits above the active variant of a
    /// data enum (at any depth). Two values are equal when their leaves are
    /// equal under these masks.
    pub fn valid_bits(&self, leaves: &[BitVec]) -> Result<Vec<BitVec>, Error> {
        fn visit(
            schema: &Schema,
            leaves: &mut std::slice::Iter<'_, BitVec>,
            out: &mut Vec<BitVec>,
        ) -> Result<(), Error> {
            match schema {
                Schema::Struct { fields, .. } => {
                    for (_, field) in fields {
                        visit(field, leaves, out)?;
                    }
                }
                Schema::Array { element, len } => {
                    for _ in 0..*len {
                        visit(element, leaves, out)?;
                    }
                }
                Schema::Tagged { .. } => {
                    let packing = Packing::new(schema)?;
                    let own: Vec<BitVec> = leaves.take(packing.leaves()).cloned().collect();
                    let (index, fields) = packing.decode(&mut own.iter().cloned())?;
                    let mut masks = Vec::new();
                    let mut field_leaves = fields.iter();
                    for (_, field) in &packing.fields[index] {
                        visit(field, &mut field_leaves, &mut masks)?;
                    }
                    let mut encoded = packing.encode(index, &masks)?;
                    encoded[0] = BitVec::ones(packing.tag_width);
                    out.extend(encoded);
                }
                leaf => {
                    leaves
                        .next()
                        .ok_or_else(|| Error("missing value leaf".into()))?;
                    out.push(BitVec::ones(leaf.width()?));
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        let mut iter = leaves.iter();
        visit(self, &mut iter, &mut out)?;
        if iter.next().is_some() {
            return Err(Error("too many value leaves".into()));
        }
        Ok(out)
    }

    pub fn leaves(&self) -> Vec<(String, Schema)> {
        fn join(path: &str, name: &str) -> String {
            if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}.{name}")
            }
        }
        fn visit(schema: &Schema, path: String, result: &mut Vec<(String, Schema)>) {
            match schema {
                Schema::Struct { fields, .. } => {
                    for (name, child) in fields {
                        visit(child, join(&path, name), result);
                    }
                }
                Schema::Array { element, len } => {
                    for index in 0..*len {
                        visit(element, join(&path, &index.to_string()), result);
                    }
                }
                Schema::Tagged { tag, .. } => {
                    visit(tag, join(&path, "tag"), result);
                    if let Some(width) = schema.payload_leaf() {
                        result.push((join(&path, "payload"), Schema::Bits(width)));
                    }
                }
                _ => result.push((path, schema.clone())),
            }
        }
        let mut result = Vec::new();
        visit(self, String::new(), &mut result);
        result
    }

    pub fn width(&self) -> Result<u16, Error> {
        match self {
            Self::Bool => Ok(1),
            Self::Bits(n) if (1..=MAX_WIDTH).contains(n) => Ok(*n),
            Self::Enum { width, .. } => Ok(*width),
            _ => Err(Error(format!(
                "expected bool, Bits<1..={MAX_WIDTH}> or enum leaf"
            ))),
        }
    }

    /// Leaf values of the zero value: all bits clear, except that enums take
    /// their first variant (data enums with zero fields, packed).
    pub fn zero_values(&self) -> Vec<BitVec> {
        fn visit(schema: &Schema, out: &mut Vec<BitVec>) {
            match schema {
                Schema::Struct { fields, .. } => {
                    for (_, field) in fields {
                        visit(field, out);
                    }
                }
                Schema::Array { element, len } => {
                    for _ in 0..*len {
                        visit(element, out);
                    }
                }
                Schema::Enum { variants, .. } => {
                    out.push(BitVec::from(variants.first().map_or(0, |(_, v)| *v)));
                }
                Schema::Tagged { payloads, .. } => {
                    let mut fields = Vec::new();
                    for (_, field) in payloads.first().map_or(&[][..], |(_, f)| f) {
                        visit(field, &mut fields);
                    }
                    match Packing::new(schema).and_then(|p| p.encode(0, &fields)) {
                        Ok(leaves) => out.extend(leaves),
                        // An invalid schema: zero leaves, rejected later.
                        Err(_) => out.extend(schema.leaves().iter().map(|_| BitVec::zero())),
                    }
                }
                _ => out.push(BitVec::zero()),
            }
        }
        let mut out = Vec::new();
        visit(self, &mut out);
        out
    }

    /// The HDL port name of the leaf at `path` in a value named `root`.
    pub(crate) fn port_name(root: &str, path: &str) -> String {
        let flat = path.replace('.', "_");
        if path.is_empty() {
            root.to_owned()
        } else if flat.starts_with(|c: char| c.is_ascii_digit()) {
            format!("{root}_{flat}")
        } else {
            flat
        }
    }
}

/// The packed layout of a data enum ([`Schema::Tagged`]), for encoding and
/// decoding leaf values. Derived `LogicValue` impls cache one per type.
#[doc(hidden)]
#[derive(Debug)]
pub struct Packing {
    name: &'static str,
    /// Each variant's name and discriminant.
    variants: Vec<(String, u64)>,
    fields: Vec<Fields>,
    /// Each variant's field leaf widths, from payload bit 0 upwards.
    widths: Vec<Vec<u16>>,
    tag_width: u16,
    payload: u16,
}

impl Packing {
    pub fn new(schema: &Schema) -> Result<Self, Error> {
        let Schema::Tagged {
            name,
            tag,
            payloads,
            ..
        } = schema
        else {
            return Err(Error("expected an enum with fields".into()));
        };
        let Schema::Enum {
            variants, width, ..
        } = &**tag
        else {
            return Err(Error("malformed enum tag".into()));
        };
        let mut widths = Vec::new();
        let mut payload = 0u32;
        for (variant, _) in payloads {
            let layout = schema.variant_layout(variant)?;
            payload = payload.max(layout.iter().map(|&(_, _, w)| u32::from(w)).sum());
            widths.push(layout.into_iter().map(|(_, _, w)| w).collect());
        }
        Ok(Self {
            name,
            variants: variants.clone(),
            fields: payloads.iter().map(|(_, f)| f.clone()).collect(),
            widths,
            tag_width: *width,
            payload: u16::try_from(payload).expect("variant_layout checks MAX_WIDTH"),
        })
    }
    /// Leaves of a value: the tag, and the payload unless it is empty.
    pub fn leaves(&self) -> usize {
        1 + usize::from(self.payload > 0)
    }
    /// The leaves of variant `index` with these field leaves; payload bits
    /// above the variant are zero.
    pub fn encode(&self, index: usize, fields: &[BitVec]) -> Result<Vec<BitVec>, Error> {
        let widths = &self.widths[index];
        if fields.len() != widths.len() {
            return Err(Error("variant field leaf count mismatch".into()));
        }
        if fields.iter().zip(widths).any(|(v, &w)| !v.fits(w)) {
            return Err(Error("value exceeds leaf width".into()));
        }
        let mut leaves = vec![BitVec::from(self.variants[index].1)];
        if self.payload > 0 {
            let parts: Vec<_> = fields.iter().zip(widths.iter().copied()).collect();
            leaves.push(BitVec::pack(&parts, self.payload));
        }
        Ok(leaves)
    }
    /// The variant index and field leaves of the next value in `values`.
    pub fn decode(
        &self,
        values: &mut impl Iterator<Item = BitVec>,
    ) -> Result<(usize, Vec<BitVec>), Error> {
        let tag = values
            .next()
            .ok_or_else(|| Error("missing value leaf".into()))?;
        let index = self
            .variants
            .iter()
            .position(|(_, d)| tag == *d)
            .ok_or_else(|| Error(format!("invalid {} discriminant", self.name)))?;
        let payload = if self.payload > 0 {
            let payload = values
                .next()
                .ok_or_else(|| Error("missing value leaf".into()))?;
            if !payload.fits(self.payload) {
                return Err(Error("value exceeds leaf width".into()));
            }
            payload
        } else {
            BitVec::zero()
        };
        let mut offset = 0;
        let fields = self.widths[index]
            .iter()
            .map(|&width| {
                let field = payload.extract(offset, width);
                offset += width;
                field
            })
            .collect();
        Ok((index, fields))
    }
}

/// Native aggregate layout. Derive this trait for named, non-generic structs
/// and enums.
pub trait LogicValue: Sized + 'static {
    fn schema() -> Schema;
    /// Leaf values in schema leaf order.
    fn flatten(&self) -> Vec<BitVec>;
    fn read(values: &mut impl Iterator<Item = BitVec>) -> Result<Self, Error>;

    /// The value of leaf values in schema leaf order (`u64`s or [`BitVec`]s).
    fn from_values<V: Clone + Into<BitVec>>(values: &[V]) -> Result<Self, Error> {
        let mut values = values.iter().cloned().map(Into::into);
        let value = Self::read(&mut values)?;
        if values.next().is_some() {
            return Err(Error("too many value leaves".into()));
        }
        Ok(value)
    }

    /// The zero value: all bits clear, except that enums take their first
    /// variant (with zero fields). Out-of-range array reads return this.
    fn zeroed() -> Self {
        Self::from_values(&Self::schema().zero_values()).expect("zero value is valid")
    }

    /// The value as a `u64` when it is a single leaf that fits one (memory
    /// addresses in cycle models). Scalars override this to avoid flattening.
    #[doc(hidden)]
    fn as_index(&self) -> Option<u64> {
        match self.flatten().as_slice() {
            [leaf] => leaf.to_u64(),
            _ => None,
        }
    }

    /// Unroll a bounded pure recurrence. The index is 0 through N-1.
    fn fold<const N: usize>(self, f: impl Fn(Self, Bits<8>) -> Self) -> Self {
        assert!(N <= 256, "fold is limited to 256 iterations");
        (0..N).fold(self, |acc, index| f(acc, bits(index as u64)))
    }
}

impl LogicValue for bool {
    fn schema() -> Schema {
        Schema::Bool
    }
    fn flatten(&self) -> Vec<BitVec> {
        vec![BitVec::from(*self)]
    }
    fn as_index(&self) -> Option<u64> {
        Some(u64::from(*self))
    }
    fn read(values: &mut impl Iterator<Item = BitVec>) -> Result<Self, Error> {
        match values.next().and_then(|v| v.to_u64()) {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(Error("expected boolean value leaf".into())),
        }
    }
}

impl<const N: usize> LogicValue for Bits<N>
where
    Width<N>: Storage,
{
    fn schema() -> Schema {
        Schema::Bits(u16::try_from(N).unwrap_or(0))
    }
    fn flatten(&self) -> Vec<BitVec> {
        vec![self.to_bitvec()]
    }
    fn as_index(&self) -> Option<u64> {
        self.to_u64()
    }
    fn read(values: &mut impl Iterator<Item = BitVec>) -> Result<Self, Error> {
        let value = values
            .next()
            .ok_or_else(|| Error("missing value leaf".into()))?;
        Self::from_bitvec(&value).ok_or_else(|| Error("value exceeds leaf width".into()))
    }
}

impl<T: LogicValue, const N: usize> LogicValue for [T; N] {
    fn schema() -> Schema {
        Schema::array::<T>(N)
    }
    fn flatten(&self) -> Vec<BitVec> {
        self.iter().flat_map(LogicValue::flatten).collect()
    }
    fn read(values: &mut impl Iterator<Item = BitVec>) -> Result<Self, Error> {
        let elements = (0..N)
            .map(|_| T::read(values))
            .collect::<Result<Vec<_>, _>>()?;
        elements
            .try_into()
            .map_err(|_| Error("array length mismatch".into()))
    }
}

/// Indexing with a run-time index, as both native Rust and logic. Reads past
/// the end return [`LogicValue::zeroed`]; writes past the end change nothing.
/// `#[logic]` functions import this trait automatically.
pub trait LogicArray: Sized {
    type Element;
    /// Element `index`, or the zero value when `index` is out of range.
    fn at<const K: usize>(&self, index: Bits<K>) -> Self::Element
    where
        Width<K>: Storage;
    /// A copy with element `index` replaced (unchanged when out of range).
    fn set<const K: usize>(self, index: Bits<K>, value: Self::Element) -> Self
    where
        Width<K>: Storage;
}

/// A run-time index as a `usize`, if it is one.
fn position<const K: usize>(index: Bits<K>) -> Option<usize>
where
    Width<K>: Storage,
{
    index.to_u64().and_then(|i| usize::try_from(i).ok())
}

impl<T: LogicValue + Clone, const N: usize> LogicArray for [T; N] {
    type Element = T;
    fn at<const K: usize>(&self, index: Bits<K>) -> T
    where
        Width<K>: Storage,
    {
        position(index)
            .and_then(|i| self.get(i))
            .cloned()
            .unwrap_or_else(T::zeroed)
    }
    fn set<const K: usize>(mut self, index: Bits<K>, value: T) -> Self
    where
        Width<K>: Storage,
    {
        if let Some(slot) = position(index).and_then(|i| self.get_mut(i)) {
            *slot = value;
        }
        self
    }
}

/// An aggregate of scalar DAG values. Used by generated code, not hardware storage.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct Symbolic {
    pub schema: Schema,
    pub values: Vec<NodeId>,
}

impl Symbolic {
    pub fn input<T: LogicValue>(builder: &mut Builder, root: &str) -> Result<Self, Error> {
        let schema = T::schema();
        let mut values = Vec::new();
        for (path, leaf) in schema.leaves() {
            let name = Schema::port_name(root, &path);
            values.push(builder.input(&name, leaf.width()?)?);
        }
        Ok(Self { schema, values })
    }
    pub fn constant<T: LogicValue>(builder: &mut Builder, value: T) -> Result<Self, Error> {
        Self::from_leaves(builder, T::schema(), &value.flatten())
    }
    fn from_leaves(
        builder: &mut Builder,
        schema: Schema,
        leaves: &[BitVec],
    ) -> Result<Self, Error> {
        let masks = schema.valid_bits(leaves)?;
        let values = schema
            .leaves()
            .iter()
            .zip(leaves.iter().zip(&masks))
            .map(|((_, leaf), (value, mask))| {
                Self::masked_constant(builder, value, mask, leaf.width()?)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { schema, values })
    }
    /// A `width`-bit constant whose bits outside `mask` are undefined: runs
    /// of valid bits are constants, runs of don't-care bits undefined nodes.
    fn masked_constant(
        builder: &mut Builder,
        value: &BitVec,
        mask: &BitVec,
        width: u16,
    ) -> Result<NodeId, Error> {
        if *mask == BitVec::ones(width) {
            return builder.constant(width, value);
        }
        let mut result: Option<NodeId> = None;
        let mut start = 0;
        while start < width {
            let valid = mask.bit(usize::from(start));
            let mut end = start + 1;
            while end < width && mask.bit(usize::from(end)) == valid {
                end += 1;
            }
            let run = if valid {
                builder.constant(end - start, value.extract(start, end - start))?
            } else {
                builder.undefined(end - start)?
            };
            result = Some(match result {
                None => run,
                Some(low) => builder.concat(run, low)?,
            });
            start = end;
        }
        result.ok_or_else(|| Error("empty constant".into()))
    }
    /// The zero value of `schema` (see [`LogicValue::zeroed`]).
    pub fn zero(builder: &mut Builder, schema: &Schema) -> Result<Self, Error> {
        Self::from_leaves(builder, schema.clone(), &schema.zero_values())
    }
    fn element_schema(&self) -> Result<(&Schema, usize), Error> {
        match &self.schema {
            Schema::Array { element, len } => Ok((element, *len)),
            _ => Err(Error("indexing requires an array".into())),
        }
    }
    /// Element `index` of an array, for a literal index.
    pub fn element(&self, index: usize) -> Result<Self, Error> {
        let (element, len) = self.element_schema()?;
        if index >= len {
            return Err(Error(format!("array index {index} out of bounds")));
        }
        let count = element.leaves().len();
        Ok(Self {
            schema: element.clone(),
            values: self.values[index * count..(index + 1) * count].to_vec(),
        })
    }
    /// An array literal; all elements must have the same type.
    pub fn array(elements: Vec<Self>) -> Result<Self, Error> {
        let Some(first) = elements.first() else {
            return Err(Error("array literals need at least one element".into()));
        };
        if elements.iter().any(|e| e.schema != first.schema) {
            return Err(Error("array element type mismatch".into()));
        }
        Ok(Self {
            schema: Schema::Array {
                element: Box::new(first.schema.clone()),
                len: elements.len(),
            },
            values: elements.iter().flat_map(|e| e.values.clone()).collect(),
        })
    }
    /// `[value; len]`.
    pub fn repeat(&self, len: usize) -> Result<Self, Error> {
        Self::array(vec![self.clone(); len])
    }
    /// The run-time index as `(node, width)`, and its value if constant.
    /// A constant index is `Some(position)`, `Some(None)` when it does not
    /// fit a `usize` (out of range).
    fn index_node(&self, builder: &Builder) -> Result<(NodeId, u16, Option<Option<usize>>), Error> {
        let Schema::Bits(width) = self.schema else {
            return Err(Error("array indexes must be Bits<N>".into()));
        };
        let node = self.scalar()?;
        let constant = builder
            .constant_value(node)
            .map(|c| c.to_u64().and_then(|i| usize::try_from(i).ok()));
        Ok((node, width, constant))
    }
    /// Element `index` of an array, or the zero value when out of range
    /// (`LogicArray::at`). A constant index selects without logic.
    pub fn at(&self, builder: &mut Builder, index: &Self) -> Result<Self, Error> {
        let (element, len) = self.element_schema()?;
        let element = element.clone();
        let (node, width, constant) = index.index_node(builder)?;
        if let Some(i) = constant {
            return match i {
                Some(i) if i < len => self.element(i),
                _ => Self::zero(builder, &element),
            };
        }
        let zero = Self::zero(builder, &element)?;
        // A binary mux tree on the low index bits, padded with zeros...
        let levels = usize::BITS - len.saturating_sub(1).leading_zeros();
        let levels = levels.min(u32::from(width));
        let mut layer = (0..1usize << levels)
            .map(|i| {
                if i < len {
                    self.element(i)
                } else {
                    Ok(zero.clone())
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        for level in 0..levels {
            let bit = Self {
                schema: Schema::Bool,
                values: vec![builder.slice(node, level as u16, 1)?],
            };
            layer = layer
                .chunks(2)
                .map(|pair| Self::mux(builder, &bit, &pair[1], &pair[0]))
                .collect::<Result<_, _>>()?;
        }
        let selected = layer.pop().expect("one element remains");
        // ...and zero when the high index bits point past the tree.
        if u32::from(width) == levels {
            return Ok(selected);
        }
        let high_width = width - levels as u16;
        let high = builder.slice(node, levels as u16, high_width)?;
        let none = builder.constant(high_width, 0)?;
        let in_range = Self {
            schema: Schema::Bool,
            values: vec![builder.binary(Binary::Eq, high, none)?],
        };
        Self::mux(builder, &in_range, &selected, &zero)
    }
    /// A copy of an array with element `index` replaced (`LogicArray::set`).
    pub fn set(&self, builder: &mut Builder, index: &Self, value: &Self) -> Result<Self, Error> {
        let (element, len) = self.element_schema()?;
        if value.schema != *element {
            return Err(Error("array element type mismatch".into()));
        }
        let (node, width, constant) = index.index_node(builder)?;
        let reachable = if width >= 64 {
            len
        } else {
            len.min(1 << width)
        };
        let mut elements = Vec::with_capacity(len);
        for i in 0..len {
            let old = self.element(i)?;
            let new = match constant {
                Some(c) if c == Some(i) => value.clone(),
                Some(_) => old,
                None if i < reachable => {
                    let at = builder.constant(width, i as u64)?;
                    let hit = Self {
                        schema: Schema::Bool,
                        values: vec![builder.binary(Binary::Eq, node, at)?],
                    };
                    Self::mux(builder, &hit, value, &old)?
                }
                None => old,
            };
            elements.push(new);
        }
        Ok(Self {
            schema: self.schema.clone(),
            values: elements.into_iter().flat_map(|e| e.values).collect(),
        })
    }
    pub fn field(&self, name: &str) -> Result<Self, Error> {
        if let Schema::Struct { fields, .. } = &self.schema {
            let mut offset = 0;
            for (field, schema) in fields {
                let count = schema.leaves().len();
                if field == name {
                    return Ok(Self {
                        schema: schema.clone(),
                        values: self.values[offset..offset + count].to_vec(),
                    });
                }
                offset += count;
            }
        }
        Err(Error(format!("unknown aggregate field {name}")))
    }
    pub fn structure<T: LogicValue>(fields: Vec<(&str, Self)>) -> Result<Self, Error> {
        let schema = T::schema();
        let Schema::Struct {
            fields: expected, ..
        } = &schema
        else {
            return Err(Error("expected struct type".into()));
        };
        let values = Self::gather(expected, &fields)?;
        Ok(Self { schema, values })
    }
    /// The leaves of `fields` in the order `expected` declares them.
    fn gather(
        expected: &[(String, Schema)],
        fields: &[(&str, Self)],
    ) -> Result<Vec<NodeId>, Error> {
        if expected.len() != fields.len() {
            return Err(Error("incomplete struct or variant value".into()));
        }
        let mut values = Vec::new();
        for (name, ty) in expected {
            let matches: Vec<_> = fields.iter().filter(|(n, _)| n == name).collect();
            if matches.len() != 1 || matches[0].1.schema != *ty {
                return Err(Error(format!("field {name} type/definition mismatch")));
            }
            values.extend(&matches[0].1.values);
        }
        Ok(values)
    }
    /// A brace or tuple constructor `path { .. }` / `path(..)`: a struct, or
    /// the variant `variant` (the path's last segment) of a data enum. A
    /// variant's field leaves are packed into the payload; the bits above
    /// them are undefined (don't-care).
    pub fn construct(
        builder: &mut Builder,
        schema: Schema,
        variant: &str,
        fields: Vec<(&str, Self)>,
    ) -> Result<Self, Error> {
        match &schema {
            Schema::Struct {
                fields: expected, ..
            } => {
                let values = Self::gather(expected, &fields)?;
                Ok(Self { schema, values })
            }
            Schema::Tagged { tag, .. } => {
                let (_, discriminant, expected) = schema.variant_fields(variant)?;
                let leaves = Self::gather(expected, &fields)?;
                let mut values = vec![builder.constant(tag.width()?, discriminant)?];
                if let Some(width) = schema.payload_leaf() {
                    values.push(Self::pack(builder, &leaves, width)?);
                }
                Ok(Self { schema, values })
            }
            _ => Err(Error(
                "constructor paths must name a struct or an enum variant with fields".into(),
            )),
        }
    }
    /// `leaves` laid out from bit 0 upwards in a `width`-bit value, the bits
    /// above them undefined.
    fn pack(builder: &mut Builder, leaves: &[NodeId], width: u16) -> Result<NodeId, Error> {
        let mut packed: Option<NodeId> = None;
        let mut used = 0;
        for &leaf in leaves {
            used += builder.node_width(leaf)?;
            packed = Some(match packed {
                None => leaf,
                Some(low) => builder.concat(leaf, low)?,
            });
        }
        if used < width {
            let padding = builder.undefined(width - used)?;
            packed = Some(match packed {
                None => padding,
                Some(low) => builder.concat(padding, low)?,
            });
        }
        packed.ok_or_else(|| Error("empty enum payload".into()))
    }
    /// The field leaves of `variant`, sliced out of a data enum value's
    /// payload (wires only; meaningful when the value is that variant).
    /// With `field`, only that field's leaves.
    fn variant_leaves(
        builder: &mut Builder,
        schema: &Schema,
        values: &[NodeId],
        variant: &str,
        field: Option<&str>,
    ) -> Result<Vec<NodeId>, Error> {
        let mut leaves = Vec::new();
        for (path, offset, width) in schema.variant_layout(variant)? {
            let selected = field.is_none_or(|f| {
                path == f
                    || path
                        .strip_prefix(f)
                        .is_some_and(|rest| rest.starts_with('.'))
            });
            if selected {
                leaves.push(builder.slice(values[1], offset, width)?);
            }
        }
        Ok(leaves)
    }
    /// For a data enum compared with another, the index of the variant when
    /// either side's tag is a constant (`None` for an invalid constant tag).
    fn known_variant(
        builder: &Builder,
        schema: &Schema,
        a: NodeId,
        b: NodeId,
    ) -> Option<Option<usize>> {
        let Schema::Tagged { tag, .. } = schema else {
            return None;
        };
        let Schema::Enum { variants, .. } = &**tag else {
            return None;
        };
        let value = builder
            .constant_value(a)
            .or_else(|| builder.constant_value(b))?;
        Some(variants.iter().position(|(_, v)| *value == *v))
    }
    /// The constant a pattern tests: a scalar pattern's value, or a data
    /// enum pattern's tag.
    pub fn pattern_key(&self, builder: &Builder, pattern: &Self) -> Option<BitVec> {
        pattern
            .values
            .first()
            .and_then(|&node| builder.constant_value(node))
            .cloned()
    }
    /// The tag a variant pattern tests.
    pub fn variant_key(&self, variant: &str) -> Option<BitVec> {
        self.schema
            .variant_fields(variant)
            .ok()
            .map(|(_, tag, _)| BitVec::from(tag))
    }
    /// A `match`: each arm's tests `(hit, tested constant)` and value, tried
    /// in order; the last arm is the default, and its tests are given when it
    /// has patterns (not `_`).
    ///
    /// When every tested constant is known and distinct, at most one test
    /// holds. If the last arm also has tests, rustc's exhaustiveness check
    /// makes exactly one hold for every valid value, so each leaf starts from
    /// its most common constant and muxes in only the arms that differ.
    /// Otherwise the default is the base, and arms whose leaf equals a
    /// constant default leaf need no mux: the rest of the chain yields it.
    pub fn select(
        builder: &mut Builder,
        mut arms: Vec<(Tests, Self)>,
        default_tests: Option<Tests>,
        default: Self,
    ) -> Result<Self, Error> {
        let exhaustive = default_tests.is_some();
        if let Some(tests) = default_tests {
            arms.push((tests, default.clone()));
        }
        let mut keys = Vec::new();
        let mut hits = Vec::new();
        for (tests, value) in &arms {
            if value.schema != default.schema {
                return Err(Error("match arm type mismatch".into()));
            }
            let mut hit: Option<Self> = None;
            for (test, key) in tests {
                keys.push(key.clone());
                hit = Some(match hit {
                    None => test.clone(),
                    Some(h) => h.binary(builder, Binary::Or, test, true)?,
                });
            }
            hits.push(hit.ok_or_else(|| Error("match arm without a pattern".into()))?);
        }
        let mut distinct: Vec<&BitVec> = keys.iter().flatten().collect();
        distinct.sort_unstable();
        distinct.dedup();
        let exclusive = distinct.len() == keys.len();
        let one_hot = exclusive && exhaustive;
        let mut values = Vec::with_capacity(default.values.len());
        for (leaf, &last) in default.values.iter().enumerate() {
            let leaves: Vec<NodeId> = arms.iter().map(|(_, v)| v.values[leaf]).collect();
            // The base: the most common constant (one-hot), else the default.
            let mut bottom = last;
            let mut skip = None;
            if one_hot {
                let mut best = (0, None);
                for &node in &leaves {
                    if let Some(c) = builder.constant_value(node) {
                        let count = leaves
                            .iter()
                            .filter(|&&n| builder.constant_value(n) == Some(c))
                            .count();
                        if count > best.0 {
                            best = (count, Some(node));
                        }
                    }
                }
                if let (_, Some(node)) = best {
                    bottom = node;
                    skip = builder.constant_value(node).cloned();
                }
            } else if exclusive {
                skip = builder.constant_value(last).cloned();
            }
            let mut result = bottom;
            // With the default among the arms (one-hot), every arm is tested.
            let tested = if one_hot {
                leaves.len()
            } else {
                leaves.len() - usize::from(exhaustive)
            };
            for (hit, &v) in hits.iter().zip(&leaves).take(tested).rev() {
                if exclusive
                    && (v == bottom
                        || (skip.is_some() && builder.constant_value(v) == skip.as_ref()))
                {
                    continue;
                }
                result = builder.mux(hit.scalar()?, v, result)?;
            }
            values.push(result);
        }
        Ok(Self {
            schema: default.schema,
            values,
        })
    }
    /// Whether a data enum value is `variant`: one comparison on its tag.
    pub fn is_variant(&self, builder: &mut Builder, variant: &str) -> Result<Self, Error> {
        let (_, discriminant, _) = self.schema.variant_fields(variant)?;
        let Schema::Tagged { tag, .. } = &self.schema else {
            unreachable!("variant_fields checked the schema");
        };
        let expected = builder.constant(tag.width()?, discriminant)?;
        Ok(Self {
            schema: Schema::Bool,
            values: vec![builder.binary(Binary::Eq, self.values[0], expected)?],
        })
    }
    /// Field `field` of variant `variant`: slices of the payload (wires
    /// only; meaningful when the value is that variant).
    pub fn variant_field(
        &self,
        builder: &mut Builder,
        variant: &str,
        field: &str,
    ) -> Result<Self, Error> {
        let (_, _, fields) = self.schema.variant_fields(variant)?;
        let Some((_, schema)) = fields.iter().find(|(name, _)| name == field) else {
            return Err(Error(format!("variant {variant} has no field {field}")));
        };
        let values =
            Self::variant_leaves(builder, &self.schema, &self.values, variant, Some(field))?;
        Ok(Self {
            schema: schema.clone(),
            values,
        })
    }
    /// A constant `match` pattern test: data enums compare only their tag,
    /// everything else compares with `==`.
    pub fn pattern_eq(&self, builder: &mut Builder, pattern: &Self) -> Result<Self, Error> {
        if matches!(self.schema, Schema::Tagged { .. }) {
            if self.schema != pattern.schema {
                return Err(Error("match pattern type mismatch".into()));
            }
            return Ok(Self {
                schema: Schema::Bool,
                values: vec![builder.binary(Binary::Eq, self.values[0], pattern.values[0])?],
            });
        }
        self.binary(builder, Binary::Eq, pattern, false)
    }
    pub fn require<T: LogicValue>(&self) -> Result<(), Error> {
        if self.schema != T::schema() {
            return Err(Error("logical value type mismatch".into()));
        }
        Ok(())
    }
    fn scalar(&self) -> Result<NodeId, Error> {
        self.schema.width()?;
        self.values
            .first()
            .copied()
            .ok_or_else(|| Error("missing scalar node".into()))
    }
    pub fn not(&self, builder: &mut Builder) -> Result<Self, Error> {
        if !matches!(self.schema, Schema::Bool | Schema::Bits(_)) {
            return Err(Error("! requires bool or Bits<N>".into()));
        }
        Ok(Self {
            schema: self.schema.clone(),
            values: vec![builder.not(self.scalar()?)?],
        })
    }
    pub fn binary(
        &self,
        builder: &mut Builder,
        op: Binary,
        rhs: &Self,
        logical: bool,
    ) -> Result<Self, Error> {
        if self.schema != rhs.schema {
            return Err(Error("binary logical type mismatch".into()));
        }
        let comparison = matches!(
            op,
            Binary::Eq | Binary::Ne | Binary::Lt | Binary::Le | Binary::Gt | Binary::Ge
        );
        match &self.schema {
            Schema::Struct { .. } | Schema::Array { .. } | Schema::Tagged { .. } => {
                return self.aggregate_equal(builder, op, rhs);
            }
            Schema::Enum { .. } if !comparison => {
                return Err(Error("enums support only comparisons".into()));
            }
            _ => {}
        }
        if logical && self.schema != Schema::Bool {
            return Err(Error("logical operators require bool".into()));
        }
        if matches!(op, Binary::Add | Binary::Sub) && self.schema == Schema::Bool {
            return Err(Error("arithmetic requires Bits<N>".into()));
        }
        let value = builder.binary(op, self.scalar()?, rhs.scalar()?)?;
        Ok(Self {
            schema: if comparison {
                Schema::Bool
            } else {
                self.schema.clone()
            },
            values: vec![value],
        })
    }
    /// `==` / `!=` on structs, arrays and data enums, as Rust's derived
    /// `PartialEq`: data enums compare their tags and the active variant's
    /// fields, never the payload bits above it.
    fn aggregate_equal(
        &self,
        builder: &mut Builder,
        op: Binary,
        rhs: &Self,
    ) -> Result<Self, Error> {
        if !matches!(op, Binary::Eq | Binary::Ne) {
            return Err(Error(
                "structs, arrays and enums with fields support only == and !=".into(),
            ));
        }
        let mut all = Self::equal(builder, &self.schema, &self.values, &rhs.values)?;
        if op == Binary::Ne {
            all = builder.not(all)?;
        }
        Ok(Self {
            schema: Schema::Bool,
            values: vec![all],
        })
    }
    /// `a == b` for leaf nodes of a value of `schema`, as one bool node.
    fn equal(
        builder: &mut Builder,
        schema: &Schema,
        a: &[NodeId],
        b: &[NodeId],
    ) -> Result<NodeId, Error> {
        fn and(builder: &mut Builder, all: Option<NodeId>, next: NodeId) -> Result<NodeId, Error> {
            match all {
                None => Ok(next),
                Some(all) => builder.binary(Binary::And, all, next),
            }
        }
        /// Each child's equality, ANDed; `true` for no children.
        fn children<'s>(
            builder: &mut Builder,
            schemas: impl Iterator<Item = &'s Schema>,
            a: &[NodeId],
            b: &[NodeId],
        ) -> Result<NodeId, Error> {
            let mut all = None;
            let mut offset = 0;
            for schema in schemas {
                let count = schema.leaves().len();
                let range = offset..offset + count;
                let same = Symbolic::equal(builder, schema, &a[range.clone()], &b[range])?;
                all = Some(and(builder, all, same)?);
                offset += count;
            }
            all.map_or_else(|| builder.constant(1, 1), Ok)
        }
        match schema {
            Schema::Struct { fields, .. } => children(builder, fields.iter().map(|(_, f)| f), a, b),
            Schema::Array { element, len } => {
                children(builder, std::iter::repeat_n(&**element, *len), a, b)
            }
            Schema::Tagged { tag, payloads, .. } => {
                let Schema::Enum { variants, .. } = &**tag else {
                    return Err(Error("malformed enum tag".into()));
                };
                let known = Self::known_variant(builder, schema, a[0], b[0]);
                let mut all = Some(builder.binary(Binary::Eq, a[0], b[0])?);
                for (index, ((variant, fields), (_, discriminant))) in
                    payloads.iter().zip(variants).enumerate()
                {
                    let skip = known.is_some_and(|k| k != Some(index));
                    if skip || fields.iter().all(|(_, f)| f.leaves().is_empty()) {
                        continue;
                    }
                    let fa = Self::variant_leaves(builder, schema, a, variant, None)?;
                    let fb = Self::variant_leaves(builder, schema, b, variant, None)?;
                    let same = children(builder, fields.iter().map(|(_, f)| f), &fa, &fb)?;
                    // With the variant unknown: (tag != variant) || fields equal.
                    let term = if known.is_some() {
                        same
                    } else {
                        let at = builder.constant(tag.width()?, *discriminant)?;
                        let is = builder.binary(Binary::Eq, a[0], at)?;
                        let other = builder.not(is)?;
                        builder.binary(Binary::Or, other, same)?
                    };
                    all = Some(and(builder, all, term)?);
                }
                Ok(all.expect("the tag comparison"))
            }
            _ => builder.binary(Binary::Eq, a[0], b[0]),
        }
    }
    pub fn mux(
        builder: &mut Builder,
        condition: &Self,
        yes: &Self,
        no: &Self,
    ) -> Result<Self, Error> {
        condition.require::<bool>()?;
        if yes.schema != no.schema {
            return Err(Error("conditional branch type mismatch".into()));
        }
        let values = yes
            .values
            .iter()
            .zip(&no.values)
            .map(|(&y, &n)| builder.mux(condition.scalar()?, y, n))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            schema: yes.schema.clone(),
            values,
        })
    }
    pub fn shift(&self, builder: &mut Builder, amount: u32, left: bool) -> Result<Self, Error> {
        if !matches!(self.schema, Schema::Bits(_)) {
            return Err(Error("shifts require Bits<N>".into()));
        }
        Ok(Self {
            schema: self.schema.clone(),
            values: vec![builder.shift(self.scalar()?, amount, left)?],
        })
    }
    pub fn slice(
        &self,
        builder: &mut Builder,
        offset: u16,
        width: u16,
        bit: bool,
    ) -> Result<Self, Error> {
        if !matches!(self.schema, Schema::Bits(_)) {
            return Err(Error("bit methods require Bits<N>".into()));
        }
        Ok(Self {
            schema: if bit {
                Schema::Bool
            } else {
                Schema::Bits(width)
            },
            values: vec![builder.slice(self.scalar()?, offset, width)?],
        })
    }
    pub fn concat(
        &self,
        builder: &mut Builder,
        rhs: &Self,
        low: u16,
        total: u16,
    ) -> Result<Self, Error> {
        let Schema::Bits(high) = self.schema else {
            return Err(Error("concat requires Bits<N>".into()));
        };
        if rhs.schema != Schema::Bits(low) || u32::from(high) + u32::from(low) != u32::from(total) {
            return Err(Error("concatenation width mismatch".into()));
        }
        Ok(Self {
            schema: Schema::Bits(total),
            values: vec![builder.concat(self.scalar()?, rhs.scalar()?)?],
        })
    }
    /// The width of a `Bits<N>` value, or `what`'s error.
    fn bits_width(&self, what: &str) -> Result<u16, Error> {
        match self.schema {
            Schema::Bits(width) => Ok(width),
            _ => Err(Error(format!("{what} requires Bits<N>"))),
        }
    }
    /// `a * b`: both `Bits<N>`, the product wrapping at N bits.
    pub fn mul(&self, builder: &mut Builder, rhs: &Self) -> Result<Self, Error> {
        let width = self.bits_width("*")?;
        if rhs.schema != self.schema {
            return Err(Error("binary logical type mismatch".into()));
        }
        Ok(Self {
            schema: self.schema.clone(),
            values: vec![builder.mul(self.scalar()?, rhs.scalar()?, width, false)?],
        })
    }
    /// `a.mul_wide::<M, O>(b)` (or `smul_wide`, `signed`): the full product,
    /// `O = N + M` bits wide.
    pub fn mul_wide(
        &self,
        builder: &mut Builder,
        rhs: &Self,
        low: u16,
        total: u16,
        signed: bool,
    ) -> Result<Self, Error> {
        let high = self.bits_width("mul_wide")?;
        if rhs.schema != Schema::Bits(low) || u32::from(high) + u32::from(low) != u32::from(total) {
            return Err(Error("product width mismatch".into()));
        }
        Ok(Self {
            schema: Schema::Bits(total),
            values: vec![builder.mul(self.scalar()?, rhs.scalar()?, total, signed)?],
        })
    }
    /// A signed comparison (`op` one of `Lt`, `Le`, `Gt`, `Ge`) of two
    /// `Bits<N>` read as two's complement: with the top bits equal it is the
    /// unsigned comparison; otherwise the negative value is the smaller.
    pub fn signed_compare(
        &self,
        builder: &mut Builder,
        op: Binary,
        rhs: &Self,
    ) -> Result<Self, Error> {
        let width = self.bits_width("signed comparison")?;
        if rhs.schema != self.schema {
            return Err(Error("binary logical type mismatch".into()));
        }
        let (a, b) = (self.scalar()?, rhs.scalar()?);
        let sa = builder.slice(a, width - 1, 1)?;
        let sb = builder.slice(b, width - 1, 1)?;
        let differ = builder.binary(Binary::Xor, sa, sb)?;
        let unsigned = builder.binary(op, a, b)?;
        // Signs differ: a < b exactly when a is the negative one.
        let less = match op {
            Binary::Lt | Binary::Le => sa,
            Binary::Gt | Binary::Ge => sb,
            _ => return Err(Error("signed comparisons are <, <=, > and >=".into())),
        };
        Ok(Self {
            schema: Schema::Bool,
            values: vec![builder.mux(differ, less, unsigned)?],
        })
    }
    /// `a.sar(amount)`: `>>` filling with copies of the top bit.
    pub fn sar(&self, builder: &mut Builder, amount: u32) -> Result<Self, Error> {
        let width = self.bits_width("sar")?;
        let a = self.scalar()?;
        let sign = builder.slice(a, width - 1, 1)?;
        let fill = |builder: &mut Builder, n: u16| -> Result<NodeId, Error> {
            let ones = builder.constant(n, BitVec::ones(n))?;
            let zeros = builder.constant(n, 0)?;
            builder.mux(sign, ones, zeros)
        };
        let value = match u16::try_from(amount) {
            Ok(0) => a,
            Ok(k) if k < width => {
                let high = fill(builder, k)?;
                let low = builder.slice(a, k, width - k)?;
                builder.concat(high, low)?
            }
            _ => fill(builder, width)?,
        };
        Ok(Self {
            schema: self.schema.clone(),
            values: vec![value],
        })
    }
    pub fn output(&self, builder: &mut Builder) -> Result<(), Error> {
        for ((path, _), &value) in self.schema.leaves().iter().zip(&self.values) {
            builder.output(&Schema::port_name("result", path), value)?;
        }
        Ok(())
    }
}
