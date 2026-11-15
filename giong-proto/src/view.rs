//! Views: decoded messages that borrow from the receive buffer.

use std::fmt;
use std::marker::PhantomData;

use crate::error::{Error, ErrorKind};
use crate::wire::{self, Reader, WireType, RECURSION_LIMIT};

/// Something that can be written in the protobuf wire format: a view, or a
/// generated message struct.
pub trait Encode {
    /// The exact number of bytes [`encode_raw`](Encode::encode_raw) appends.
    fn encoded_len(&self) -> usize;

    /// Appends the message's fields to `buf`, without a length prefix.
    ///
    /// Use [`encode`](Encode::encode) unless the size was already checked.
    fn encode_raw(&self, buf: &mut Vec<u8>);

    /// Appends the message to `buf`, without a length prefix.
    ///
    /// Fails with `GIONG-E4008` when the message is larger than 2 GiB,
    /// which protobuf cannot frame; `buf` is then left as it was.
    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), Error> {
        let len = self.encoded_len();
        if len > i32::MAX as usize {
            return Err(Error::new(ErrorKind::TooLarge, 0, 0));
        }
        buf.reserve(len);
        self.encode_raw(buf);
        Ok(())
    }

    /// The message as a new `Vec`.
    fn encode_to_vec(&self) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::new();
        self.encode(&mut buf)?;
        Ok(buf)
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    #[inline]
    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
    #[inline]
    fn encode_raw(&self, buf: &mut Vec<u8>) {
        (**self).encode_raw(buf)
    }
}

/// A decoded message that borrows its strings, bytes and repeated fields
/// from the input buffer.
///
/// Generated `...View<'a>` types implement this. Decoding validates the
/// whole message, nested messages included, so every accessor after it is
/// infallible.
///
/// A view's [`Encode`] writes the bytes it was decoded from, unchanged: a
/// view cannot be modified, so those bytes are its encoding (forwarding a
/// received message costs one copy). [`encode_canonical`] writes it field
/// by field instead.
///
/// [`encode_canonical`]: View::encode_canonical
pub trait View<'a>: Default + Clone + Encode {
    /// Merges one encoded occurrence of the message into `self`, the way
    /// protobuf merges a message that appears twice: scalars and strings
    /// are replaced, repeated fields appended, nested messages merged.
    ///
    /// `depth` is how many more levels of nesting are allowed.
    fn merge(&mut self, buf: &'a [u8], depth: u32) -> Result<(), Error>;

    /// Decodes a message from `buf`.
    ///
    /// Runs in one pass over the input with no heap allocation, except
    /// when the same singular nested message appears more than once in the
    /// input (which encoders never produce).
    fn decode(buf: &'a [u8]) -> Result<Self, Error> {
        let mut view = Self::default();
        view.merge(buf, RECURSION_LIMIT)?;
        Ok(view)
    }

    /// The exact number of bytes [`encode_canonical`](View::encode_canonical)
    /// appends.
    fn canonical_len(&self) -> usize;

    /// Appends the message field by field, in field-number order, the way
    /// Google's libraries write it: one occurrence per singular field,
    /// repeated fields packed where the `.proto` says so, then the unknown
    /// fields in the order they arrived. Slower than [`Encode::encode`];
    /// for normalising messages and for tests.
    fn encode_canonical(&self, buf: &mut Vec<u8>);
}

/// Bytes the runs in `src` take. Generated views' [`Encode::encoded_len`].
#[inline]
pub fn spans_len(src: &Spans<'_>) -> usize {
    src.as_slice().iter().map(|s| s.len()).sum()
}

/// Appends the runs in `src`. Generated views' [`Encode::encode_raw`].
#[inline]
pub fn put_spans(src: &Spans<'_>, buf: &mut Vec<u8>) {
    for s in src.as_slice() {
        buf.extend_from_slice(s);
    }
}

/// The parts of the input a field or message was read from: usually one
/// slice, several only when a message was merged from more than one
/// occurrence.
#[derive(Clone, Default)]
pub enum Spans<'a> {
    /// Nothing was read.
    #[default]
    None,
    /// One run of the input.
    One(&'a [u8]),
    /// Several runs, in input order.
    Many(Vec<&'a [u8]>),
}

/// Where `inner` starts inside `outer`, if it lies inside it.
#[inline(always)]
fn offset_in(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let o = outer.as_ptr() as usize;
    let i = inner.as_ptr() as usize;
    if i >= o && i + inner.len() <= o + outer.len() {
        Some(i - o)
    } else {
        None
    }
}

impl<'a> Spans<'a> {
    /// Adds `seg[start..end]`, joining it with the last run when both lie
    /// in `seg`.
    #[inline]
    pub fn extend(&mut self, seg: &'a [u8], start: usize, end: usize) {
        match self {
            Spans::None => *self = Spans::One(&seg[start..end]),
            Spans::One(s) => match offset_in(seg, s) {
                Some(off) => *s = &seg[off..end],
                None => *self = Spans::Many(vec![*s, &seg[start..end]]),
            },
            Spans::Many(v) => {
                let last = v.last_mut().expect("Many is never empty");
                match offset_in(seg, last) {
                    Some(off) => *last = &seg[off..end],
                    None => v.push(&seg[start..end]),
                }
            }
        }
    }

    /// Adds a whole segment as its own run.
    #[inline]
    pub fn push(&mut self, seg: &'a [u8]) {
        match self {
            Spans::None => *self = Spans::One(seg),
            Spans::One(s) => *self = Spans::Many(vec![*s, seg]),
            Spans::Many(v) => v.push(seg),
        }
    }

    /// Whether nothing was read.
    #[inline]
    pub fn is_empty(&self) -> bool {
        matches!(self, Spans::None)
    }

    /// The runs, in order.
    #[inline]
    pub fn as_slice(&self) -> &[&'a [u8]] {
        match self {
            Spans::None => &[],
            Spans::One(s) => std::slice::from_ref(s),
            Spans::Many(v) => v,
        }
    }
}

impl fmt::Debug for Spans<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total: usize = self.as_slice().iter().map(|s| s.len()).sum();
        write!(f, "Spans({} runs, {} bytes)", self.as_slice().len(), total)
    }
}

/// A generated Rust enum for a protobuf enum.
pub trait EnumType: Copy + fmt::Debug + 'static {
    /// The variant with this number.
    fn from_i32(v: i32) -> Option<Self>;
    /// The variant's number.
    fn to_i32(self) -> i32;
}

/// The value of an enum field: a number that may or may not be one of the
/// enum's variants (proto3 enums are open; a newer sender may use values
/// this code does not know).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum<E> {
    raw: i32,
    _e: PhantomData<fn() -> E>,
}

impl<E: EnumType> Enum<E> {
    /// The value with this number.
    #[inline(always)]
    pub const fn from_raw(raw: i32) -> Enum<E> {
        Enum {
            raw,
            _e: PhantomData,
        }
    }

    /// The number on the wire.
    #[inline(always)]
    pub fn raw(self) -> i32 {
        self.raw
    }

    /// The variant, or `None` for a number the enum does not define.
    #[inline(always)]
    pub fn known(self) -> Option<E> {
        E::from_i32(self.raw)
    }
}

impl<E> Default for Enum<E> {
    fn default() -> Self {
        Enum {
            raw: 0,
            _e: PhantomData,
        }
    }
}

impl<E: EnumType> From<E> for Enum<E> {
    fn from(e: E) -> Self {
        Enum::from_raw(e.to_i32())
    }
}

impl<E: EnumType + PartialEq> PartialEq<E> for Enum<E> {
    fn eq(&self, other: &E) -> bool {
        self.raw == other.to_i32()
    }
}

impl<E: EnumType> fmt::Debug for Enum<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.known() {
            Some(e) => write!(f, "{e:?}"),
            None => write!(f, "{}", self.raw),
        }
    }
}

/// A field type, as the repeated, map and lazy-message fields of views see
/// it. Generated code names these (`Repeated<'a, Int32>`); applications only
/// see the `Out` values.
pub trait Kind<'a> {
    /// The value an accessor or iterator returns.
    type Out: Clone + Default;
    /// The wire type of one value.
    const WIRE: WireType;
    /// Width of one value when packed: 0 for varints, 4 or 8 for fixed
    /// widths, `usize::MAX` when the type cannot be packed.
    const PACKED: usize;
    /// Whether [`keep`](Kind::keep) can return false (closed enums only).
    const FILTERS: bool = false;

    /// Reads one value whose tag (with wire type `WIRE`) was just read.
    fn read(r: &mut Reader<'a>, field: u32, depth: u32) -> Result<Self::Out, Error>;

    /// Reads one value into a slot that may hold an earlier one: replaces
    /// it, or merges into it for messages.
    #[inline(always)]
    fn read_into(
        slot: &mut Self::Out,
        r: &mut Reader<'a>,
        field: u32,
        depth: u32,
    ) -> Result<(), Error> {
        *slot = Self::read(r, field, depth)?;
        Ok(())
    }

    /// Whether a value read from the wire belongs to the field. False only
    /// for numbers a closed (proto2) enum does not define: protobuf keeps
    /// those as unknown fields.
    #[inline(always)]
    fn keep(_v: &Self::Out) -> bool {
        true
    }

    /// Bytes the value takes, without its tag.
    fn len(v: &Self::Out) -> usize;

    /// Appends the value, without its tag.
    fn put(v: &Self::Out, buf: &mut Vec<u8>);

    /// Appends the value with its tag. Groups override this.
    #[inline(always)]
    fn put_field(field: u32, v: &Self::Out, buf: &mut Vec<u8>) {
        wire::put_tag(buf, field, Self::WIRE);
        Self::put(v, buf);
    }

    /// Bytes the value takes with its tag.
    #[inline(always)]
    fn field_len(field: u32, v: &Self::Out) -> usize {
        wire::tag_len(field) + Self::len(v)
    }
}

macro_rules! varint_kind {
    ($(#[$doc:meta])* $name:ident, $out:ty, |$r:ident| $read:expr, |$v:ident| $enc:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name;
        impl<'a> Kind<'a> for $name {
            type Out = $out;
            const WIRE: WireType = WireType::Varint;
            const PACKED: usize = 0;
            #[inline(always)]
            fn read($r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<$out, Error> {
                Ok($read)
            }
            #[inline(always)]
            fn len($v: &$out) -> usize {
                wire::varint_len($enc)
            }
            #[inline(always)]
            fn put($v: &$out, buf: &mut Vec<u8>) {
                wire::put_varint(buf, $enc)
            }
        }
    };
}

varint_kind!(/// `int32`.
    Int32, i32, |r| r.varint()? as i32, |v| *v as i64 as u64);
varint_kind!(/// `int64`.
    Int64, i64, |r| r.varint()? as i64, |v| *v as u64);
varint_kind!(/// `uint32`.
    UInt32, u32, |r| r.varint()? as u32, |v| *v as u64);
varint_kind!(/// `uint64`.
    UInt64, u64, |r| r.varint()?, |v| *v);
varint_kind!(/// `sint32`.
    SInt32, i32, |r| wire::unzigzag32(r.varint()?), |v| wire::zigzag32(*v));
varint_kind!(/// `sint64`.
    SInt64, i64, |r| wire::unzigzag64(r.varint()?), |v| wire::zigzag64(*v));
varint_kind!(/// `bool`.
    Bool, bool, |r| r.varint()? != 0, |v| *v as u64);

macro_rules! fixed_kind {
    ($(#[$doc:meta])* $name:ident, $out:ty, $wire:ident, $w:expr, |$r:ident| $read:expr, |$v:ident, $buf:ident| $put:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name;
        impl<'a> Kind<'a> for $name {
            type Out = $out;
            const WIRE: WireType = WireType::$wire;
            const PACKED: usize = $w;
            #[inline(always)]
            fn read($r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<$out, Error> {
                Ok($read)
            }
            #[inline(always)]
            fn len(_v: &$out) -> usize {
                $w
            }
            #[inline(always)]
            fn put($v: &$out, $buf: &mut Vec<u8>) {
                $put
            }
        }
    };
}

fixed_kind!(/// `fixed32`.
    Fixed32, u32, I32, 4, |r| r.fixed32()?, |v, buf| wire::put_fixed32(buf, *v));
fixed_kind!(/// `fixed64`.
    Fixed64, u64, I64, 8, |r| r.fixed64()?, |v, buf| wire::put_fixed64(buf, *v));
fixed_kind!(/// `sfixed32`.
    SFixed32, i32, I32, 4, |r| r.fixed32()? as i32, |v, buf| wire::put_fixed32(buf, *v as u32));
fixed_kind!(/// `sfixed64`.
    SFixed64, i64, I64, 8, |r| r.fixed64()? as i64, |v, buf| wire::put_fixed64(buf, *v as u64));
fixed_kind!(/// `float`.
    Float, f32, I32, 4, |r| f32::from_bits(r.fixed32()?), |v, buf| wire::put_fixed32(buf, v.to_bits()));
fixed_kind!(/// `double`.
    Double, f64, I64, 8, |r| f64::from_bits(r.fixed64()?), |v, buf| wire::put_fixed64(buf, v.to_bits()));

/// An open (proto3) enum.
pub struct OpenEnum<E>(PhantomData<fn() -> E>);

impl<'a, E: EnumType> Kind<'a> for OpenEnum<E> {
    type Out = Enum<E>;
    const WIRE: WireType = WireType::Varint;
    const PACKED: usize = 0;
    #[inline(always)]
    fn read(r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<Enum<E>, Error> {
        Ok(Enum::from_raw(r.varint()? as i32))
    }
    #[inline(always)]
    fn len(v: &Enum<E>) -> usize {
        wire::varint_len(v.raw() as i64 as u64)
    }
    #[inline(always)]
    fn put(v: &Enum<E>, buf: &mut Vec<u8>) {
        wire::put_varint(buf, v.raw() as i64 as u64)
    }
}

/// A closed (proto2) enum: numbers it does not define are kept as unknown
/// fields, not as the field's value.
pub struct ClosedEnum<E>(PhantomData<fn() -> E>);

impl<'a, E: EnumType> Kind<'a> for ClosedEnum<E> {
    type Out = Enum<E>;
    const WIRE: WireType = WireType::Varint;
    const PACKED: usize = 0;
    const FILTERS: bool = true;
    #[inline(always)]
    fn read(r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<Enum<E>, Error> {
        Ok(Enum::from_raw(r.varint()? as i32))
    }
    #[inline(always)]
    fn keep(v: &Enum<E>) -> bool {
        v.known().is_some()
    }
    #[inline(always)]
    fn len(v: &Enum<E>) -> usize {
        wire::varint_len(v.raw() as i64 as u64)
    }
    #[inline(always)]
    fn put(v: &Enum<E>, buf: &mut Vec<u8>) {
        wire::put_varint(buf, v.raw() as i64 as u64)
    }
}

/// `string`: borrowed from the input, checked to be UTF-8 once at decode.
#[derive(Debug, Clone, Copy)]
pub struct Str;

impl<'a> Kind<'a> for Str {
    type Out = &'a str;
    const WIRE: WireType = WireType::Len;
    const PACKED: usize = usize::MAX;
    #[inline(always)]
    fn read(r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<&'a str, Error> {
        r.str()
    }
    #[inline(always)]
    fn len(v: &&'a str) -> usize {
        wire::len_delimited(v.len())
    }
    #[inline(always)]
    fn put(v: &&'a str, buf: &mut Vec<u8>) {
        wire::put_varint(buf, v.len() as u64);
        buf.extend_from_slice(v.as_bytes());
    }
}

/// `bytes`: borrowed from the input.
#[derive(Debug, Clone, Copy)]
pub struct Bytes;

impl<'a> Kind<'a> for Bytes {
    type Out = &'a [u8];
    const WIRE: WireType = WireType::Len;
    const PACKED: usize = usize::MAX;
    #[inline(always)]
    fn read(r: &mut Reader<'a>, _field: u32, _depth: u32) -> Result<&'a [u8], Error> {
        r.bytes()
    }
    #[inline(always)]
    fn len(v: &&'a [u8]) -> usize {
        wire::len_delimited(v.len())
    }
    #[inline(always)]
    fn put(v: &&'a [u8], buf: &mut Vec<u8>) {
        wire::put_varint(buf, v.len() as u64);
        buf.extend_from_slice(v);
    }
}

/// A nested message.
pub struct Message<V>(PhantomData<fn() -> V>);

impl<'a, V: View<'a>> Kind<'a> for Message<V> {
    type Out = V;
    const WIRE: WireType = WireType::Len;
    const PACKED: usize = usize::MAX;
    #[inline]
    fn read(r: &mut Reader<'a>, field: u32, depth: u32) -> Result<V, Error> {
        let mut v = V::default();
        Self::read_into(&mut v, r, field, depth)?;
        Ok(v)
    }
    #[inline]
    fn read_into(slot: &mut V, r: &mut Reader<'a>, _field: u32, depth: u32) -> Result<(), Error> {
        let b = r.bytes()?;
        if depth == 0 {
            return Err(r.error(ErrorKind::TooDeep));
        }
        slot.merge(b, depth - 1)
    }
    #[inline]
    fn len(v: &V) -> usize {
        wire::len_delimited(v.canonical_len())
    }
    #[inline]
    fn put(v: &V, buf: &mut Vec<u8>) {
        wire::put_varint(buf, v.canonical_len() as u64);
        v.encode_canonical(buf);
    }
}

/// A proto2 group: a nested message framed by start and end tags.
pub struct Group<V>(PhantomData<fn() -> V>);

impl<'a, V: View<'a>> Kind<'a> for Group<V> {
    type Out = V;
    const WIRE: WireType = WireType::StartGroup;
    const PACKED: usize = usize::MAX;
    #[inline]
    fn read(r: &mut Reader<'a>, field: u32, depth: u32) -> Result<V, Error> {
        let mut v = V::default();
        Self::read_into(&mut v, r, field, depth)?;
        Ok(v)
    }
    #[inline]
    fn read_into(slot: &mut V, r: &mut Reader<'a>, field: u32, depth: u32) -> Result<(), Error> {
        let b = r.group(field, depth)?;
        slot.merge(b, depth - 1)
    }
    #[inline]
    fn len(v: &V) -> usize {
        v.canonical_len()
    }
    #[inline]
    fn put(v: &V, buf: &mut Vec<u8>) {
        v.encode_canonical(buf);
    }
    #[inline]
    fn put_field(field: u32, v: &V, buf: &mut Vec<u8>) {
        wire::put_tag(buf, field, WireType::StartGroup);
        v.encode_canonical(buf);
        wire::put_tag(buf, field, WireType::EndGroup);
    }
    #[inline]
    fn field_len(field: u32, v: &V) -> usize {
        2 * wire::tag_len(field) + v.canonical_len()
    }
}

/// A repeated field, read lazily from the input: decoding counts and checks
/// the values, iterating reads them again from the buffer. Nothing is
/// collected into a `Vec`.
pub struct Repeated<'a, K> {
    spans: Spans<'a>,
    field: u32,
    len: u32,
    _k: PhantomData<fn() -> K>,
}

impl<'a, K> Default for Repeated<'a, K> {
    fn default() -> Self {
        Repeated {
            spans: Spans::None,
            field: 0,
            len: 0,
            _k: PhantomData,
        }
    }
}

impl<'a, K> Clone for Repeated<'a, K> {
    fn clone(&self) -> Self {
        Repeated {
            spans: self.spans.clone(),
            field: self.field,
            len: self.len,
            _k: PhantomData,
        }
    }
}

impl<'a, K: Kind<'a>> Repeated<'a, K> {
    /// Whether a value of this field may come with this wire type.
    #[inline(always)]
    pub fn accepts(wire: WireType) -> bool {
        wire == K::WIRE || (wire == WireType::Len && K::PACKED != usize::MAX)
    }

    /// Reads one occurrence whose tag started at `start` in `seg`: a value,
    /// or a packed run. Called by generated `merge` code.
    #[inline]
    pub fn read(
        &mut self,
        r: &mut Reader<'a>,
        start: usize,
        field: u32,
        wire: WireType,
        depth: u32,
    ) -> Result<(), Error> {
        let n = if wire == WireType::Len && K::PACKED != usize::MAX {
            let (body, n) = if K::PACKED == 0 {
                r.packed_varints()?
            } else {
                r.packed_fixed(K::PACKED)?
            };
            if K::FILTERS {
                let mut p = Reader::new(body);
                let mut kept = 0;
                while !p.is_empty() {
                    let v = K::read(&mut p, field, depth)?;
                    kept += K::keep(&v) as u32;
                }
                kept
            } else {
                n
            }
        } else {
            let v = K::read(r, field, depth)?;
            K::keep(&v) as u32
        };
        self.field = field;
        self.len += n;
        self.spans.extend(r.buf(), start, r.pos());
        Ok(())
    }

    /// How many values the field holds.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the field holds no values.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The values, in order, read from the buffer as the iterator advances.
    #[inline]
    pub fn iter(&self) -> RepeatedIter<'a, '_, K> {
        RepeatedIter {
            spans: self.spans.as_slice(),
            reader: Reader::new(&[]),
            packed: None,
            field: self.field,
            left: self.len,
            _k: PhantomData,
        }
    }

    /// The first value, or `None` when the field is empty.
    pub fn first(&self) -> Option<K::Out> {
        self.iter().next()
    }

    /// The values collected into a `Vec` (allocates).
    pub fn to_vec(&self) -> Vec<K::Out> {
        self.iter().collect()
    }

    /// Bytes the field takes when encoded: packed when `packed` is true and
    /// the type can be packed, one tag per value otherwise.
    pub fn encoded_len(&self, packed: bool) -> usize {
        if self.len == 0 {
            return 0;
        }
        if packed && K::PACKED != usize::MAX {
            let body: usize = if K::PACKED == 0 {
                self.iter().map(|v| K::len(&v)).sum()
            } else {
                K::PACKED * self.len()
            };
            wire::tag_len(self.field) + wire::len_delimited(body)
        } else {
            self.iter().map(|v| K::field_len(self.field, &v)).sum()
        }
    }

    /// Appends the field, packed when `packed` is true and the type can be
    /// packed.
    pub fn encode_raw(&self, packed: bool, buf: &mut Vec<u8>) {
        if self.len == 0 {
            return;
        }
        if packed && K::PACKED != usize::MAX {
            let body: usize = if K::PACKED == 0 {
                self.iter().map(|v| K::len(&v)).sum()
            } else {
                K::PACKED * self.len()
            };
            wire::put_tag(buf, self.field, WireType::Len);
            wire::put_varint(buf, body as u64);
            for v in self.iter() {
                K::put(&v, buf);
            }
        } else {
            for v in self.iter() {
                K::put_field(self.field, &v, buf);
            }
        }
    }
}

impl<'a, K: Kind<'a>> fmt::Debug for Repeated<'a, K>
where
    K::Out: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a, 's, K: Kind<'a>> IntoIterator for &'s Repeated<'a, K> {
    type Item = K::Out;
    type IntoIter = RepeatedIter<'a, 's, K>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over a [`Repeated`] field.
pub struct RepeatedIter<'a, 's, K> {
    spans: &'s [&'a [u8]],
    reader: Reader<'a>,
    packed: Option<Reader<'a>>,
    field: u32,
    left: u32,
    _k: PhantomData<fn() -> K>,
}

impl<'a, K: Kind<'a>> Iterator for RepeatedIter<'a, '_, K> {
    type Item = K::Out;

    #[inline]
    fn next(&mut self) -> Option<K::Out> {
        if self.left == 0 {
            return None;
        }
        loop {
            if let Some(p) = &mut self.packed {
                while !p.is_empty() {
                    // Checked at decode time; cannot fail.
                    let v = K::read(p, self.field, RECURSION_LIMIT).unwrap_or_default();
                    if K::keep(&v) {
                        self.left -= 1;
                        return Some(v);
                    }
                }
                self.packed = None;
            }
            while self.reader.is_empty() {
                let (first, rest) = self.spans.split_first()?;
                self.reader = Reader::new(first);
                self.spans = rest;
            }
            let r = &mut self.reader;
            let Ok(Some((field, wire))) = r.tag() else {
                return None;
            };
            if field == self.field {
                if wire == K::WIRE {
                    let v = K::read(r, field, RECURSION_LIMIT).unwrap_or_default();
                    if K::keep(&v) {
                        self.left -= 1;
                        return Some(v);
                    }
                    continue;
                }
                if wire == WireType::Len && K::PACKED != usize::MAX {
                    let body = r.bytes().unwrap_or_default();
                    self.packed = Some(Reader::new(body));
                    continue;
                }
            }
            if r.skip(field, wire, RECURSION_LIMIT).is_err() {
                return None;
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

impl<'a, K: Kind<'a>> ExactSizeIterator for RepeatedIter<'a, '_, K> {}

/// A singular message field read lazily: decoding checks it, the accessor
/// decodes it again from the buffer. Used where a message contains itself
/// (directly or through others), so views stay a fixed size without boxes.
pub struct Lazy<'a, V> {
    spans: Spans<'a>,
    _v: PhantomData<fn() -> V>,
}

impl<'a, V> Default for Lazy<'a, V> {
    fn default() -> Self {
        Lazy {
            spans: Spans::None,
            _v: PhantomData,
        }
    }
}

impl<'a, V> Clone for Lazy<'a, V> {
    fn clone(&self) -> Self {
        Lazy {
            spans: self.spans.clone(),
            _v: PhantomData,
        }
    }
}

impl<'a, V: View<'a>> Lazy<'a, V> {
    /// Reads one occurrence of the field, a length-delimited message.
    #[inline]
    pub fn read(&mut self, r: &mut Reader<'a>, depth: u32) -> Result<(), Error> {
        let b = r.bytes()?;
        if depth == 0 {
            return Err(r.error(ErrorKind::TooDeep));
        }
        V::default().merge(b, depth - 1)?;
        self.spans.push(b);
        Ok(())
    }

    /// Reads one occurrence of the field, a group.
    #[inline]
    pub fn read_group(&mut self, r: &mut Reader<'a>, field: u32, depth: u32) -> Result<(), Error> {
        let b = r.group(field, depth)?;
        V::default().merge(b, depth - 1)?;
        self.spans.push(b);
        Ok(())
    }

    /// Whether the field was present.
    #[inline(always)]
    pub fn is_some(&self) -> bool {
        !self.spans.is_empty()
    }

    /// The message, or `None` when the field was absent.
    #[inline]
    pub fn get(&self) -> Option<V> {
        if self.spans.is_empty() {
            return None;
        }
        let mut v = V::default();
        for s in self.spans.as_slice() {
            // Checked at decode time; cannot fail.
            let _ = v.merge(s, RECURSION_LIMIT);
        }
        Some(v)
    }

    /// Clears the field (a oneof switching to another case does this).
    #[inline]
    pub fn clear(&mut self) {
        self.spans = Spans::None;
    }
}

impl<'a, V: View<'a> + fmt::Debug> fmt::Debug for Lazy<'a, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// One entry of a map field, as a message with the key in field 1 and the
/// value in field 2.
pub struct MapEntry<'a, K: Kind<'a>, V: Kind<'a>> {
    /// The key (its default when the entry has none).
    pub key: K::Out,
    /// The value (its default when the entry has none).
    pub value: V::Out,
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Default for MapEntry<'a, K, V> {
    fn default() -> Self {
        MapEntry {
            key: K::Out::default(),
            value: V::Out::default(),
        }
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Clone for MapEntry<'a, K, V> {
    fn clone(&self) -> Self {
        MapEntry {
            key: self.key.clone(),
            value: self.value.clone(),
        }
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Encode for MapEntry<'a, K, V> {
    fn encoded_len(&self) -> usize {
        K::field_len(1, &self.key) + V::field_len(2, &self.value)
    }
    fn encode_raw(&self, buf: &mut Vec<u8>) {
        K::put_field(1, &self.key, buf);
        V::put_field(2, &self.value, buf);
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> View<'a> for MapEntry<'a, K, V> {
    fn canonical_len(&self) -> usize {
        self.encoded_len()
    }
    fn encode_canonical(&self, buf: &mut Vec<u8>) {
        self.encode_raw(buf)
    }
    fn merge(&mut self, buf: &'a [u8], depth: u32) -> Result<(), Error> {
        let mut r = Reader::new(buf);
        while let Some((field, wire)) = r.tag()? {
            match (field, wire) {
                (1, w) if w == K::WIRE => K::read_into(&mut self.key, &mut r, 1, depth)?,
                (2, w) if w == V::WIRE => V::read_into(&mut self.value, &mut r, 2, depth)?,
                (f, w) => r.skip(f, w, depth)?,
            }
        }
        Ok(())
    }
}

/// A map field: entries read lazily from the input, in wire order. When a
/// key appears twice the last entry wins, as protobuf requires; [`get`]
/// honours that, iteration shows every entry.
///
/// [`get`]: Map::get
pub struct Map<'a, K: Kind<'a>, V: Kind<'a>> {
    entries: Repeated<'a, Message<MapEntry<'a, K, V>>>,
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Default for Map<'a, K, V> {
    fn default() -> Self {
        Map {
            entries: Repeated::default(),
        }
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Clone for Map<'a, K, V> {
    fn clone(&self) -> Self {
        Map {
            entries: self.entries.clone(),
        }
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> Map<'a, K, V> {
    /// Reads one entry. Called by generated `merge` code.
    #[inline]
    pub fn read(
        &mut self,
        r: &mut Reader<'a>,
        start: usize,
        field: u32,
        depth: u32,
    ) -> Result<(), Error> {
        self.entries.read(r, start, field, WireType::Len, depth)
    }

    /// How many entries the input held (a repeated key counts each time).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry as `(key, value)`, in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (K::Out, V::Out)> + '_ {
        self.entries.iter().map(|e| (e.key, e.value))
    }

    /// The value for `key`: the last entry with that key. A linear scan;
    /// collect into a `HashMap` to look up many keys in a large map.
    pub fn get(&self, key: &K::Out) -> Option<V::Out>
    where
        K::Out: PartialEq,
    {
        let mut found = None;
        for e in self.entries.iter() {
            if &e.key == key {
                found = Some(e.value);
            }
        }
        found
    }

    /// Bytes the field takes when encoded. Maps are never packed; the
    /// argument keeps the signature the same as [`Repeated::encoded_len`].
    pub fn encoded_len(&self, _packed: bool) -> usize {
        self.entries.encoded_len(false)
    }

    /// Appends the field.
    pub fn encode_raw(&self, _packed: bool, buf: &mut Vec<u8>) {
        self.entries.encode_raw(false, buf)
    }
}

impl<'a, K: Kind<'a>, V: Kind<'a>> fmt::Debug for Map<'a, K, V>
where
    K::Out: fmt::Debug,
    V::Out: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// How the unknown-field pass sees a field of a message.
#[derive(Clone, Copy)]
pub enum FieldClass {
    /// A field of the message with a wire type it accepts.
    Known,
    /// Not a field of the message, or a wire type it does not accept.
    Unknown,
    /// A closed enum field: values the enum defines are known, others not.
    ClosedEnum(fn(i32) -> bool),
}

/// Bytes the unknown fields in `src` take: every field `class` calls
/// unknown, plus closed-enum values the enum does not define.
pub fn unknown_len(src: &Spans<'_>, class: fn(u32, WireType) -> FieldClass) -> usize {
    let mut n = 0;
    for_each_unknown(src, class, |piece| match piece {
        Piece::Raw(bytes) => n += bytes.len(),
        Piece::Varint(field, v) => n += wire::tag_len(field) + wire::varint_len(v),
    });
    n
}

/// Appends the unknown fields in `src`, in input order.
pub fn put_unknown(src: &Spans<'_>, class: fn(u32, WireType) -> FieldClass, buf: &mut Vec<u8>) {
    for_each_unknown(src, class, |piece| match piece {
        Piece::Raw(bytes) => buf.extend_from_slice(bytes),
        Piece::Varint(field, v) => {
            wire::put_tag(buf, field, WireType::Varint);
            wire::put_varint(buf, v);
        }
    });
}

enum Piece<'b> {
    Raw(&'b [u8]),
    Varint(u32, u64),
}

fn for_each_unknown(
    src: &Spans<'_>,
    class: fn(u32, WireType) -> FieldClass,
    mut out: impl FnMut(Piece<'_>),
) {
    for seg in src.as_slice() {
        let mut r = Reader::new(seg);
        loop {
            let start = r.pos();
            let Ok(Some((field, wire))) = r.tag() else {
                break;
            };
            let body_start = r.pos();
            if r.skip(field, wire, RECURSION_LIMIT).is_err() {
                break;
            }
            match class(field, wire) {
                FieldClass::Known => {}
                FieldClass::Unknown => out(Piece::Raw(&seg[start..r.pos()])),
                FieldClass::ClosedEnum(known) => {
                    let mut v = Reader::new(&seg[body_start..r.pos()]);
                    if wire == WireType::Len {
                        let Ok(body) = v.bytes() else { continue };
                        let mut p = Reader::new(body);
                        while !p.is_empty() {
                            let Ok(x) = p.varint() else { break };
                            if !known(x as i32) {
                                out(Piece::Varint(field, x));
                            }
                        }
                    } else if let Ok(x) = v.varint() {
                        if !known(x as i32) {
                            out(Piece::Raw(&seg[start..r.pos()]));
                        }
                    }
                }
            }
        }
    }
}

/// Bytes a repeated field of `values` takes. Generated message structs
/// call this.
#[inline]
pub fn repeated_len<'a, K: Kind<'a>>(field: u32, values: &[K::Out], packed: bool) -> usize {
    if values.is_empty() {
        return 0;
    }
    if packed && K::PACKED != usize::MAX {
        let body = if K::PACKED == 0 {
            values.iter().map(|v| K::len(v)).sum()
        } else {
            K::PACKED * values.len()
        };
        wire::tag_len(field) + wire::len_delimited(body)
    } else {
        values.iter().map(|v| K::field_len(field, v)).sum()
    }
}

/// Appends a repeated field of `values`. Generated message structs call
/// this.
#[inline]
pub fn put_repeated<'a, K: Kind<'a>>(
    field: u32,
    values: &[K::Out],
    packed: bool,
    buf: &mut Vec<u8>,
) {
    if values.is_empty() {
        return;
    }
    if packed && K::PACKED != usize::MAX {
        let body = if K::PACKED == 0 {
            values.iter().map(|v| K::len(v)).sum()
        } else {
            K::PACKED * values.len()
        };
        wire::put_tag(buf, field, WireType::Len);
        wire::put_varint(buf, body as u64);
        for v in values {
            K::put(v, buf);
        }
    } else {
        for v in values {
            K::put_field(field, v, buf);
        }
    }
}

/// Bytes a nested message field takes, tag included.
#[inline]
pub fn message_len<M: Encode + ?Sized>(field: u32, m: &M) -> usize {
    wire::tag_len(field) + wire::len_delimited(m.encoded_len())
}

/// Appends a nested message field: tag, length, message.
#[inline]
pub fn put_message<M: Encode + ?Sized>(field: u32, m: &M, buf: &mut Vec<u8>) {
    wire::put_tag(buf, field, WireType::Len);
    wire::put_varint(buf, m.encoded_len() as u64);
    m.encode_raw(buf);
}

/// Bytes a group field takes, both tags included.
#[inline]
pub fn group_len<M: Encode + ?Sized>(field: u32, m: &M) -> usize {
    2 * wire::tag_len(field) + m.encoded_len()
}

/// Appends a group field: start tag, message, end tag.
#[inline]
pub fn put_group<M: Encode + ?Sized>(field: u32, m: &M, buf: &mut Vec<u8>) {
    wire::put_tag(buf, field, WireType::StartGroup);
    m.encode_raw(buf);
    wire::put_tag(buf, field, WireType::EndGroup);
}
