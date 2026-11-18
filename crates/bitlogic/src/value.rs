//! Structured values and the symbolic values used by generated frontend code.
use crate::{Binary, Bits, Builder, Error, NodeId, bits};
use std::any::{TypeId, type_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schema {
    Bool,
    Bits(u8),
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
        width: u8,
        variants: Vec<(String, u64)>,
    },
}

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
        let width = (64 - largest.leading_zeros()).max(1) as u8;
        Self::Enum {
            identity: TypeId::of::<T>(),
            name: type_name::<T>(),
            width,
            variants,
        }
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
                _ => result.push((path, schema.clone())),
            }
        }
        let mut result = Vec::new();
        visit(self, String::new(), &mut result);
        result
    }

    pub fn width(&self) -> Result<u8, Error> {
        match self {
            Self::Bool => Ok(1),
            Self::Bits(n) if (1..=64).contains(n) => Ok(*n),
            Self::Enum { width, .. } => Ok(*width),
            _ => Err(Error("expected bool, Bits<1..=64> or enum leaf".into())),
        }
    }

    /// Leaf values of the zero value: all bits clear, except that enums take
    /// their first variant.
    pub fn zero_values(&self) -> Vec<u64> {
        self.leaves()
            .iter()
            .map(|(_, leaf)| match leaf {
                Self::Enum { variants, .. } => variants.first().map_or(0, |(_, v)| *v),
                _ => 0,
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

/// Native aggregate layout. Derive this trait for named, non-generic structs.
pub trait LogicValue: Sized + 'static {
    fn schema() -> Schema;
    fn flatten(&self) -> Vec<u64>;
    fn read(values: &mut impl Iterator<Item = u64>) -> Result<Self, Error>;

    fn from_values(values: &[u64]) -> Result<Self, Error> {
        let mut values = values.iter().copied();
        let value = Self::read(&mut values)?;
        if values.next().is_some() {
            return Err(Error("too many value leaves".into()));
        }
        Ok(value)
    }

    /// The zero value: all bits clear, except that enums take their first
    /// variant. Out-of-range array reads return this.
    fn zeroed() -> Self {
        Self::from_values(&Self::schema().zero_values()).expect("zero value is valid")
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
    fn flatten(&self) -> Vec<u64> {
        vec![u64::from(*self)]
    }
    fn read(values: &mut impl Iterator<Item = u64>) -> Result<Self, Error> {
        match values.next() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(Error("expected boolean value leaf".into())),
        }
    }
}

impl<const N: usize> LogicValue for Bits<N> {
    fn schema() -> Schema {
        Schema::Bits(u8::try_from(N).unwrap_or(0))
    }
    fn flatten(&self) -> Vec<u64> {
        vec![self.get()]
    }
    fn read(values: &mut impl Iterator<Item = u64>) -> Result<Self, Error> {
        let width = Self::schema().width()?;
        let value = values
            .next()
            .ok_or_else(|| Error("missing value leaf".into()))?;
        if value & !(u64::MAX >> (64 - width)) != 0 {
            return Err(Error("value exceeds leaf width".into()));
        }
        Ok(bits(value))
    }
}

impl<T: LogicValue, const N: usize> LogicValue for [T; N] {
    fn schema() -> Schema {
        Schema::array::<T>(N)
    }
    fn flatten(&self) -> Vec<u64> {
        self.iter().flat_map(LogicValue::flatten).collect()
    }
    fn read(values: &mut impl Iterator<Item = u64>) -> Result<Self, Error> {
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
    fn at<const K: usize>(&self, index: Bits<K>) -> Self::Element;
    /// A copy with element `index` replaced (unchanged when out of range).
    fn set<const K: usize>(self, index: Bits<K>, value: Self::Element) -> Self;
}

impl<T: LogicValue + Clone, const N: usize> LogicArray for [T; N] {
    type Element = T;
    fn at<const K: usize>(&self, index: Bits<K>) -> T {
        usize::try_from(index.get())
            .ok()
            .and_then(|i| self.get(i))
            .cloned()
            .unwrap_or_else(T::zeroed)
    }
    fn set<const K: usize>(mut self, index: Bits<K>, value: T) -> Self {
        if let Some(slot) = usize::try_from(index.get())
            .ok()
            .and_then(|i| self.get_mut(i))
        {
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
    fn from_leaves(builder: &mut Builder, schema: Schema, leaves: &[u64]) -> Result<Self, Error> {
        let values = schema
            .leaves()
            .iter()
            .zip(leaves)
            .map(|((_, leaf), &value)| builder.constant(leaf.width()?, value))
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
    fn index_node(&self, builder: &Builder) -> Result<(NodeId, u8, Option<u64>), Error> {
        let Schema::Bits(width) = self.schema else {
            return Err(Error("array indexes must be Bits<N>".into()));
        };
        let node = self.scalar()?;
        Ok((node, width, builder.constant_value(node)))
    }
    /// Element `index` of an array, or the zero value when out of range
    /// (`LogicArray::at`). A constant index selects without logic.
    pub fn at(&self, builder: &mut Builder, index: &Self) -> Result<Self, Error> {
        let (element, len) = self.element_schema()?;
        let element = element.clone();
        let (node, width, constant) = index.index_node(builder)?;
        if let Some(i) = constant {
            return match usize::try_from(i) {
                Ok(i) if i < len => self.element(i),
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
                values: vec![builder.slice(node, level as u8, 1)?],
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
        let high_width = width - levels as u8;
        let high = builder.slice(node, levels as u8, high_width)?;
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
                Some(c) if c == i as u64 => value.clone(),
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
        if expected.len() != fields.len() {
            return Err(Error("incomplete struct value".into()));
        }
        let mut values = Vec::new();
        for (name, ty) in expected {
            let matches: Vec<_> = fields.iter().filter(|(n, _)| n == name).collect();
            if matches.len() != 1 || matches[0].1.schema != *ty {
                return Err(Error(format!(
                    "struct field {name} type/definition mismatch"
                )));
            }
            values.extend(&matches[0].1.values);
        }
        Ok(Self { schema, values })
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
            Schema::Struct { .. } | Schema::Array { .. } => {
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
    /// `==` / `!=` on structs and arrays: every leaf equal.
    fn aggregate_equal(
        &self,
        builder: &mut Builder,
        op: Binary,
        rhs: &Self,
    ) -> Result<Self, Error> {
        if !matches!(op, Binary::Eq | Binary::Ne) {
            return Err(Error("structs and arrays support only == and !=".into()));
        }
        let mut all = builder.constant(1, 1)?;
        for (&a, &b) in self.values.iter().zip(&rhs.values) {
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
        offset: u8,
        width: u8,
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
        low: u8,
        total: u8,
    ) -> Result<Self, Error> {
        let Schema::Bits(high) = self.schema else {
            return Err(Error("concat requires Bits<N>".into()));
        };
        if rhs.schema != Schema::Bits(low) || u16::from(high) + u16::from(low) != u16::from(total) {
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
