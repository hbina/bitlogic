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
    /// An enum whose variants carry fields. Leaves are the `tag` (a
    /// [`Schema::Enum`] of every variant), then each variant's fields in
    /// declaration order under `<Variant>.<field>`; tuple fields are named
    /// `0`, `1`, ... Fields of inactive variants hold their zero values.
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

    /// For a data enum: the variant's discriminant, the leaf offset of its
    /// first field and its fields.
    fn variant_fields(&self, variant: &str) -> Result<(u64, usize, &Fields), Error> {
        let Self::Tagged { tag, payloads, .. } = self else {
            return Err(Error("expected an enum with fields".into()));
        };
        let Self::Enum { variants, .. } = &**tag else {
            return Err(Error("malformed enum tag".into()));
        };
        let mut offset = 1;
        for ((name, fields), (_, value)) in payloads.iter().zip(variants) {
            if name == variant {
                return Ok((*value, offset, fields));
            }
            offset += fields.iter().map(|(_, f)| f.leaves().len()).sum::<usize>();
        }
        Err(Error(format!("unknown enum variant {variant}")))
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
                Schema::Tagged { tag, payloads, .. } => {
                    visit(tag, join(&path, "tag"), result);
                    for (variant, fields) in payloads {
                        let base = join(&path, variant);
                        for (name, child) in fields {
                            visit(child, join(&base, name), result);
                        }
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
    /// their first variant.
    pub fn zero_values(&self) -> Vec<BitVec> {
        self.leaves()
            .iter()
            .map(|(_, leaf)| match leaf {
                Self::Enum { variants, .. } => {
                    BitVec::from(variants.first().map_or(0, |(_, v)| *v))
                }
                _ => BitVec::zero(),
            })
            .collect()
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
        let values = schema
            .leaves()
            .iter()
            .zip(leaves)
            .map(|((_, leaf), value)| builder.constant(leaf.width()?, value))
            .collect::<Result<_, _>>()?;
        Ok(Self { schema, values })
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
    /// the variant `variant` (the path's last segment) of a data enum.
    /// Fields of the other variants are zero.
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
                let (discriminant, offset, expected) = schema.variant_fields(variant)?;
                let payload = Self::gather(expected, &fields)?;
                let leaves = schema.leaves();
                let zeros = schema.zero_values();
                let mut values = Vec::with_capacity(leaves.len());
                values.push(builder.constant(tag.width()?, discriminant)?);
                for (i, (_, leaf)) in leaves.iter().enumerate().skip(1) {
                    values.push(if (offset..offset + payload.len()).contains(&i) {
                        payload[i - offset]
                    } else {
                        builder.constant(leaf.width()?, &zeros[i])?
                    });
                }
                Ok(Self { schema, values })
            }
            _ => Err(Error(
                "constructor paths must name a struct or an enum variant with fields".into(),
            )),
        }
    }
    /// For two data enum values, one with a constant tag: the leaf range of
    /// that variant's fields.
    fn known_variant(
        &self,
        builder: &Builder,
        rhs: &Self,
    ) -> Result<Option<std::ops::Range<usize>>, Error> {
        let Schema::Tagged { tag, payloads, .. } = &self.schema else {
            return Ok(None);
        };
        let Schema::Enum { variants, .. } = &**tag else {
            return Ok(None);
        };
        let constant = builder
            .constant_value(self.values[0])
            .or_else(|| builder.constant_value(rhs.values[0]));
        let Some(value) = constant else {
            return Ok(None);
        };
        let Some(((name, fields), _)) = payloads
            .iter()
            .zip(variants)
            .find(|(_, (_, v))| *value == *v)
        else {
            return Ok(None);
        };
        let (_, offset, _) = self.schema.variant_fields(name)?;
        let count: usize = fields.iter().map(|(_, f)| f.leaves().len()).sum();
        Ok(Some(offset..offset + count))
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
            .map(|(tag, _, _)| BitVec::from(tag))
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
        let (discriminant, _, _) = self.schema.variant_fields(variant)?;
        let Schema::Tagged { tag, .. } = &self.schema else {
            unreachable!("variant_fields checked the schema");
        };
        let expected = builder.constant(tag.width()?, discriminant)?;
        Ok(Self {
            schema: Schema::Bool,
            values: vec![builder.binary(Binary::Eq, self.values[0], expected)?],
        })
    }
    /// Field `field` of variant `variant` (wires only; meaningful when the
    /// value is that variant).
    pub fn variant_field(&self, variant: &str, field: &str) -> Result<Self, Error> {
        let (_, mut offset, fields) = self.schema.variant_fields(variant)?;
        for (name, schema) in fields {
            let count = schema.leaves().len();
            if name == field {
                return Ok(Self {
                    schema: schema.clone(),
                    values: self.values[offset..offset + count].to_vec(),
                });
            }
            offset += count;
        }
        Err(Error(format!("variant {variant} has no field {field}")))
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
    /// `==` / `!=` on structs, arrays and data enums: every leaf equal
    /// (inactive variant fields are always zero, so this is Rust's `==`).
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
        let pairs: Vec<(NodeId, NodeId)> = match self.known_variant(builder, rhs)? {
            // Against a value whose variant is known, only the tag and that
            // variant's fields can differ: other fields are zero on both sides.
            Some(range) => std::iter::once(0)
                .chain(range)
                .map(|i| (self.values[i], rhs.values[i]))
                .collect(),
            None => self
                .values
                .iter()
                .copied()
                .zip(rhs.values.iter().copied())
                .collect(),
        };
        let mut all = builder.constant(1, 1)?;
        for (a, b) in pairs {
            let same = builder.binary(Binary::Eq, a, b)?;
            all = builder.binary(Binary::And, all, same)?;
        }
        if op == Binary::Ne {
            all = builder.not(all)?;
        }
        Ok(Self {
            schema: Schema::Bool,
            values: vec![all],
        })
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
    pub fn output(&self, builder: &mut Builder) -> Result<(), Error> {
        for ((path, _), &value) in self.schema.leaves().iter().zip(&self.values) {
            builder.output(&Schema::port_name("result", path), value)?;
        }
        Ok(())
    }
}
