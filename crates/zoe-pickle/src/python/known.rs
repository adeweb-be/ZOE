//! The three objects of other classes that a heap of
//! [`Heap::with_values`] models: a `DateTime`, a `Message` and
//! `Missing.Value`.

use super::{Heap, Instance, Made, Object, ObjectId, Undecided, Unsupported, Value};

/// The class of a `DateTime`, as its GLOBAL names it.
const DATE_TIME: (&str, &str) = ("DateTime.DateTime", "DateTime");
/// The class of a `Message`, as its GLOBAL names it.
pub(super) const MESSAGE: (&str, &str) = ("zope.i18nmessageid.message", "Message");
/// The module of `Missing.Value`. Its GLOBAL is `Missing.V`.
const MISSING: &str = "Missing";
/// The first and the last instant of a `DateTime` that the model takes, in
/// microseconds since the epoch: 1900-01-01 and the end of 2999. In this
/// range `DateTime.__setstate__` gives a state for each zone of the table.
const DATE_TIME_MICROS: std::ops::RangeInclusive<i128> =
    -2_208_988_800_000_000..=32_503_679_999_999_999;
/// The most arguments that `Message` takes.
const MESSAGE_ARGS: usize = 7;

/// An object of a class that a heap of [`Heap::with_values`] models.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Known {
    /// A `DateTime` that NEWOBJ made, before BUILD gives it its state.
    NewDateTime,
    /// A `DateTime`: `_micros`, and the name of `_tz`.
    DateTime(i128, &'static str),
    /// A `Message`.
    Message,
}

/// What a comparison sees of a value of a heap of [`Heap::with_values`].
#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    /// A `DateTime`: `_micros` and `_tz`.
    DateTime(i128, &'a str),
    /// A `Message`: its text. It compares as a str.
    Message(&'a str),
    /// `Missing.Value`.
    Missing,
}

impl Heap {
    /// Compares two values for `==` when one of them is a `DateTime`, a
    /// `Message` or `Missing.Value`. `None` when neither is.
    ///
    /// The left value compares first, as in Python. `DateTime.__eq__` and
    /// `Missing.__eq__` give false for an object of another class. A
    /// `Message` compares as a str. A persistent reference raises.
    pub(super) fn known_eq(&self, a: Value, b: Value) -> Option<Result<bool, Undecided>> {
        let (p, q) = (self.kind(a), self.kind(b));
        if p.is_none() && q.is_none() {
            return None;
        }
        if let (Value::Object(x), Value::Object(y)) = (a, b)
            && x == y
        {
            return Some(Ok(true));
        }
        let reference = |value: Value| self.reference(value).is_some();
        Some(match (p, q) {
            (Some(Kind::DateTime(m, z)), Some(Kind::DateTime(n, w))) => Ok(m == n && z == w),
            (Some(Kind::Message(s)), Some(Kind::Message(t))) => Ok(s == t),
            (Some(Kind::Message(s)), None) => str_eq(s, self.str(b), reference(b)),
            (None, Some(Kind::Message(t))) => str_eq(t, self.str(a), reference(a)),
            (Some(Kind::DateTime(..) | Kind::Missing), _) => Ok(false),
            // A str, a `Message` or a value of a built-in type gives
            // `NotImplemented`, then the right object gives false.
            (_, Some(Kind::DateTime(..) | Kind::Missing)) => {
                if reference(a) {
                    Err(Undecided::Raises)
                } else {
                    Ok(false)
                }
            }
            (None, None) => unreachable!("one value is known"),
        })
    }

    /// Compares two values for `<` when one of them is a `DateTime`, a
    /// `Message` or `Missing.Value`. `None` when neither is.
    ///
    /// `Missing.__lt__` gives false, and `Missing.__gt__` gives true for
    /// another object: a value of a built-in type is below `Missing.Value`.
    /// `DateTime.lessThan` compares `_micros`, and compares that int with
    /// an object without `_micros`, so a str or a tuple raises there.
    /// `None` is below a `DateTime`. A `Message` compares as a str.
    pub(super) fn known_lt(&self, a: Value, b: Value) -> Option<Result<bool, Undecided>> {
        let (p, q) = (self.kind(a), self.kind(b));
        if p.is_none() && q.is_none() {
            return None;
        }
        let reference = |value: Value| self.reference(value).is_some();
        Some(match (p, q) {
            (Some(Kind::Missing), _) => Ok(false),
            (Some(Kind::DateTime(m, _)), Some(Kind::DateTime(n, _))) => Ok(m < n),
            (Some(Kind::DateTime(..) | Kind::Message(_)), Some(Kind::Missing)) => Ok(true),
            (Some(Kind::DateTime(..)), Some(Kind::Message(_)))
            | (Some(Kind::Message(_)), Some(Kind::DateTime(..))) => Err(Undecided::Raises),
            (Some(Kind::Message(s)), Some(Kind::Message(t))) => Ok(s < t),
            (Some(Kind::Message(s)), None) => match self.str(b) {
                Some(t) => Ok(s < t),
                None if reference(b) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::Message(t))) => match self.str(a) {
                Some(s) => Ok(s < t),
                None if reference(a) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (Some(Kind::DateTime(..)), None) => match self.builtin(b) {
                Some(Builtin::None) => Ok(false),
                Some(Builtin::Number) => Err(Undecided::Unknown),
                Some(Builtin::Other | Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::DateTime(..))) => match self.builtin(a) {
                Some(Builtin::None) => Ok(true),
                Some(Builtin::Number) => Err(Undecided::Unknown),
                Some(Builtin::Other | Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::Missing)) => match self.builtin(a) {
                Some(Builtin::None | Builtin::Number | Builtin::Other) => Ok(true),
                Some(Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, None) => unreachable!("one value is known"),
        })
    }

    /// Returns the kind of a value that is not a `DateTime`, a `Message`
    /// or `Missing.Value`, for the comparisons with them. `None` for a
    /// class or an object of another class.
    fn builtin(&self, value: Value) -> Option<Builtin> {
        match value {
            Value::None => Some(Builtin::None),
            Value::Bool(_) | Value::Int(_) | Value::Float(_) => Some(Builtin::Number),
            Value::Object(id) => match self.get(id) {
                Object::Str(_)
                | Object::Bytes(_)
                | Object::Tuple(_)
                | Object::List(_)
                | Object::Dict(_) => Some(Builtin::Other),
                Object::Reference(_) => Some(Builtin::Reference),
                Object::Class { .. } | Object::Instance(_) | Object::Set { .. } => None,
            },
        }
    }
}

impl Heap {
    /// Returns the class of a GLOBAL. A heap of [`Heap::with_values`]
    /// refuses another name of the module `Missing`: Python can give
    /// `Missing.Value` for it, and the model would keep two objects.
    pub(super) fn global(&mut self, module: &str, name: &str) -> Result<Value, Unsupported> {
        if self.values && module == MISSING && name != "V" {
            return Err(Unsupported("a name of Missing other than V"));
        }
        Ok(self.new_class(module, name))
    }

    /// Returns the `Message` that REDUCE makes of the class `class` and the
    /// arguments `args`, with the arguments that `Message.__reduce__` gives
    /// again: a new tuple of 7 items, a new str of the text, the domain, the
    /// default, a new dict of the mapping or `None`, the plural, the default
    /// plural and the number. Missing items are `None`.
    pub(super) fn new_message(&mut self, class: Value, args: Value) -> Result<Value, Unsupported> {
        let other = Unsupported("a Message of another form");
        let items = self.tuple(args).ok_or(other)?.to_vec();
        if items.is_empty() || items.len() > MESSAGE_ARGS {
            return Err(other);
        }
        let text = self.str(items[0]).ok_or(other)?.to_owned();
        let mut fresh = Vec::with_capacity(MESSAGE_ARGS);
        // `str(self)` copies the text. Only the empty str is shared.
        fresh.push(if text.is_empty() {
            self.str_object("")
        } else {
            Value::Object(self.push(Object::Str(text)))
        });
        for index in 1..MESSAGE_ARGS {
            let item = items.get(index).copied().unwrap_or(Value::None);
            fresh.push(match index {
                // `Message` keeps a mapping in a proxy, and `__reduce__`
                // copies it to a new dict.
                3 => match item {
                    Value::None => Value::None,
                    _ => {
                        let mapping = self.dict(item).ok_or(other)?.to_vec();
                        self.new_dict(mapping)
                    }
                },
                // `Message` takes a number only if it is an int or a float.
                6 => match item {
                    Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => item,
                    _ => return Err(other),
                },
                _ => item,
            });
        }
        let args = self.new_tuple(fresh);
        let id = self.push(Object::Instance(Instance {
            made: Made::Reduce,
            class,
            args,
            state: None,
        }));
        self.known.insert(id, Known::Message);
        Ok(Value::Object(id))
    }

    /// Returns the `DateTime` that NEWOBJ makes of the class `class` and the
    /// arguments `args`. Its BUILD comes next.
    pub(super) fn new_date_time(
        &mut self,
        class: Value,
        args: Value,
    ) -> Result<Value, Unsupported> {
        if self.class(class) != Some(DATE_TIME) || self.tuple(args) != Some(&[]) {
            return Err(Unsupported("a NEWOBJ that the model does not model"));
        }
        let id = self.push(Object::Instance(Instance {
            made: Made::New,
            class,
            args,
            state: None,
        }));
        self.known.insert(id, Known::NewDateTime);
        Ok(Value::Object(id))
    }

    /// Gives the `DateTime` of `id` the state that `__setstate__` leaves
    /// of `state`, as `__getstate__` gives it again: a new tuple of
    /// `_micros`, `_timezone_naive` and `_tz`. `_tz` is the str of the
    /// table of `DateTime` for the name of the zone: one object for each
    /// name, in the whole heap.
    pub(super) fn build_date_time(
        &mut self,
        id: ObjectId,
        state: Value,
    ) -> Result<(), Unsupported> {
        let other = Unsupported("a DateTime of another form");
        if self.known.get(&id) != Some(&Known::NewDateTime) {
            return Err(Unsupported("a BUILD of another form"));
        }
        let &[micros, naive, zone] = self.tuple(state).ok_or(other)? else {
            return Err(other);
        };
        let (Value::Int(micros), Value::Bool(naive)) = (micros, naive) else {
            return Err(other);
        };
        if !DATE_TIME_MICROS.contains(&micros) {
            return Err(Unsupported("a DateTime out of the range of the model"));
        }
        let zone = self
            .str(zone)
            .and_then(crate::zones::canonical)
            .ok_or(Unsupported("a DateTime of a zone that is not in the table"))?;
        let text = match self.zones.get(zone) {
            Some(&text) => text,
            None => {
                let text = self.push(Object::Str(zone.to_owned()));
                self.zones.insert(zone, text);
                text
            }
        };
        let state = self.new_tuple(vec![
            Value::Int(micros),
            Value::Bool(naive),
            Value::Object(text),
        ]);
        let Object::Instance(instance) = &mut self.objects[id.0 as usize] else {
            unreachable!("a known DateTime is an instance");
        };
        instance.state = Some(state);
        self.known.insert(id, Known::DateTime(micros, zone));
        Ok(())
    }

    /// Returns what a comparison sees of `value`, when it is an object that
    /// a heap of [`Heap::with_values`] models.
    fn kind(&self, value: Value) -> Option<Kind<'_>> {
        if !self.values {
            return None;
        }
        let Value::Object(id) = value else {
            return None;
        };
        match (self.known.get(&id), self.get(id)) {
            (Some(&Known::DateTime(micros, zone)), _) => Some(Kind::DateTime(micros, zone)),
            (Some(Known::Message), Object::Instance(instance)) => {
                let text = self.tuple(instance.args).and_then(|args| self.str(args[0]));
                Some(Kind::Message(text.expect("a Message holds its text")))
            }
            (None, Object::Class { module, name }) if module == MISSING && name == "V" => {
                Some(Kind::Missing)
            }
            _ => None,
        }
    }
}

/// The kinds of values of [`Heap::builtin`].
#[derive(Clone, Copy)]
enum Builtin {
    None,
    /// A bool, an int or a float.
    Number,
    /// A str, a bytes, a tuple, a list or a dict.
    Other,
    /// A persistent reference.
    Reference,
}

/// `s == other` for the text `s` of a `Message`: `str.__eq__`. `text` is the
/// text of `other` if it is a str. A persistent reference raises in its
/// `__eq__`. Another object is not equal.
fn str_eq(s: &str, text: Option<&str>, reference: bool) -> Result<bool, Undecided> {
    match text {
        Some(t) => Ok(s == t),
        None if reference => Err(Undecided::Raises),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::decode_record;
    use crate::python::tests::{CLASS, dump, load};

    /// A record whose state is a tuple of 20 values: `None`, `1`, `1.5`,
    /// `True`, `'s'`, `'Page'`, `b'b'`, `(1,)`, `[1]`, `{}`,
    /// `Missing.Value`, a `DateTime` `a` in `GMT+2`, `a` in `UTC`, a
    /// `DateTime` one second later in `UTC`, `Message('Page', 'plone')`,
    /// `Message('Page', 'plone.app.contenttypes')`, the class `int`,
    /// `Message('a')`, `'a'` and `Message('')`, as ZODB 6 writes it with
    /// Python 3.13. ZODB's resolver pickles it again to the same bytes.
    const VALUES: &[u8] =
        b"\x80\x03cBTrees.OOBTree\nOOBucket\nq\x00.\x80\x03(NK\x01G?\xf8\x00\x00\x00\x00\x00\x00\
        \x88X\x01\x00\x00\x00sq\x01X\x04\x00\x00\x00Pageq\x02C\x01bq\x03K\x01\x85q\x04]q\x05K\
        \x01a}q\x06cMissing\nV\nq\x07cDateTime.DateTime\nDateTime\nq\x08)\x81q\x09\x8a\x07\x00\
        @ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\n\x87q\x0bbh\x08)\x81q\x0c\x8a\x07\x00@ FHG\x06\
        \x89X\x03\x00\x00\x00UTCq\x0d\x87q\x0ebh\x08)\x81q\x0f\x8a\x07@\x82/FHG\x06\x89h\x0d\
        \x87q\x10bczope.i18nmessageid.message\nMessage\nq\x11(X\x04\x00\x00\x00Pageq\x12X\x05\
        \x00\x00\x00ploneq\x13NNNNNtq\x14Rq\x15h\x11(X\x04\x00\x00\x00Pageq\x16X\x16\x00\x00\
        \x00plone.app.contenttypesq\x17NNNNNtq\x18Rq\x19cbuiltins\nint\nq\x1ah\x11(X\x01\x00\
        \x00\x00aq\x1bNNNNNNtq\x1cRq\x1dX\x01\x00\x00\x00aq\x1eh\x11(X\x00\x00\x00\x00q\x1fNNN\
        NNNtq Rq!tq\".";

    /// `x < y` in Python 3.13 for the values of `VALUES`, row `x`, column
    /// `y`: `T` true, `F` false, `R` raises.
    const LT: [&str; 20] = [
        "RRRRRRRRRRTTTTRRRRRR",
        "RFTFRRRRRRTTTTRRRRRR",
        "RFFFRRRRRRTTTTRRRRRR",
        "RFTFRRRRRRTTTTRRRRRR",
        "RRRRFFRRRRTRRRFFRFFF",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRRRFRRRTRRRRRRRRR",
        "RRRRRRRFRRTRRRRRRRRR",
        "RRRRRRRRFRTRRRRRRRRR",
        "RRRRRRRRRRTRRRRRRRRR",
        "FFFFFFFFFFFFFFFFFFFF",
        "FFFFRRRRRRTFFTRRRRRR",
        "FFFFRRRRRRTFFTRRRRRR",
        "FFFFRRRRRRTFFFRRRRRR",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRRRRRRRTRRRRRRRRR",
        "RRRRTFRRRRTRRRFFRFFF",
        "RRRRTFRRRRTRRRFFRFFF",
        "RRRRTTRRRRTRRRTTRTTF",
    ];

    /// `x == y` in Python 3.13 for the values of `VALUES`.
    const EQ: [&str; 20] = [
        "TFFFFFFFFFFFFFFFFFFF",
        "FTFTFFFFFFFFFFFFFFFF",
        "FFTFFFFFFFFFFFFFFFFF",
        "FTFTFFFFFFFFFFFFFFFF",
        "FFFFTFFFFFFFFFFFFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFFTFFFFFFFFFFFFF",
        "FFFFFFFTFFFFFFFFFFFF",
        "FFFFFFFFTFFFFFFFFFFF",
        "FFFFFFFFFTFFFFFFFFFF",
        "FFFFFFFFFFTFFFFFFFFF",
        "FFFFFFFFFFFTFFFFFFFF",
        "FFFFFFFFFFFFTFFFFFFF",
        "FFFFFFFFFFFFFTFFFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFFFFFFFFFFFFTFFF",
        "FFFFFFFFFFFFFFFFFTTF",
        "FFFFFFFFFFFFFFFFFTTF",
        "FFFFFFFFFFFFFFFFFFFT",
    ];

    /// Returns the answer of a comparison as a letter of `LT` and `EQ`, or
    /// `?` for no answer.
    fn letter(answer: Result<bool, Undecided>) -> char {
        match answer {
            Ok(true) => 'T',
            Ok(false) => 'F',
            Err(Undecided::Raises) => 'R',
            Err(_) => '?',
        }
    }

    #[test]
    fn a_heap_with_values_pickles_them_as_zodbs_resolver_does() {
        let mut heap = Heap::with_values();
        let (class, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        assert_eq!(dump(&heap, class, state), VALUES);
        // The default heap and the heap of instances do not take them so.
        let tree = decode_record(VALUES).unwrap();
        assert_eq!(Heap::new().load(&tree), Err(Unsupported("an opaque node")));
    }

    #[test]
    fn values_compare_as_python_compares_them() {
        let mut heap = Heap::with_values();
        let (_, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        let values = heap.tuple(state).unwrap().to_vec();
        let mut decided = 0;
        for (i, &x) in values.iter().enumerate() {
            for (j, &y) in values.iter().enumerate() {
                let python = (LT[i].as_bytes()[j] as char, EQ[i].as_bytes()[j] as char);
                let ours = (letter(heap.lt(x, y)), letter(heap.eq(x, y)));
                // No answer is always allowed. An answer is the answer of
                // Python.
                for (ours, python) in [(ours.0, python.0), (ours.1, python.1)] {
                    assert!(ours == '?' || ours == python, "{i} {j}: {ours} {python}");
                    decided += usize::from(ours != '?');
                }
            }
        }
        // Each `==` with a DateTime, a Message or Missing.Value has an
        // answer, and each `<` of them that the metadata of a catalog
        // makes.
        let known = [10, 11, 12, 13, 14, 15, 17, 19];
        for &i in &known {
            for j in 0..values.len() {
                assert_ne!(letter(heap.eq(values[i], values[j])), '?', "{i} {j}");
                assert_ne!(letter(heap.eq(values[j], values[i])), '?', "{j} {i}");
            }
        }
        for (i, j) in [
            (10, 11),
            (11, 10),
            (11, 12),
            (12, 13),
            (13, 11),
            (0, 11),
            (11, 0),
            (4, 11),
            (11, 4),
            (14, 5),
            (5, 14),
            (14, 15),
            (14, 10),
            (10, 14),
            (7, 10),
            (4, 10),
            (0, 10),
            (11, 14),
            (14, 11),
            (19, 17),
        ] {
            assert_ne!(letter(heap.lt(values[i], values[j])), '?', "{i} {j}");
        }
        assert!(decided > 500, "{decided}");
    }

    #[test]
    fn a_datetime_keeps_its_zone_and_its_instant() {
        let mut heap = Heap::with_values();
        let (_, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        let values = heap.tuple(state).unwrap().to_vec();
        let (a, a_utc, b) = (values[11], values[12], values[13]);
        // One instant in two zones: not equal, neither is below the other.
        assert_eq!(heap.compare(a, a_utc), Ok(Ordering::Greater));
        assert_eq!(heap.compare(a_utc, a), Ok(Ordering::Greater));
        assert_eq!(heap.compare(a_utc, b), Ok(Ordering::Less));
        // The str of a zone is one object for each name, across records.
        let mut other = Heap::with_values();
        let (_, first) = other.load(&decode_record(VALUES).unwrap()).unwrap();
        let (_, second) = other.load(&decode_record(VALUES).unwrap()).unwrap();
        let zone = |heap: &Heap, value: Value| {
            let state = heap.instance(value).unwrap().state.unwrap();
            heap.tuple(state).unwrap()[2]
        };
        let (x, y) = (
            other.tuple(first).unwrap()[12],
            other.tuple(second).unwrap()[13],
        );
        assert_eq!(zone(&other, x), zone(&other, y));
        assert_ne!(x, y);
    }

    #[test]
    fn a_heap_with_values_refuses_what_it_does_not_model() {
        let date_time = |state: &[u8]| -> Vec<u8> {
            [
                &b"\x80\x03cDateTime.DateTime\nDateTime\n)\x81"[..],
                state,
                b"b.",
            ]
            .concat()
        };
        let instant = b"\x8a\x07\x00@ FHG\x06";
        for (state, what) in [
            (
                date_time(&[&instant[..], b"\x89X\x03\x00\x00\x00EST\x87"].concat()),
                "a DateTime of a zone that is not in the table",
            ),
            (
                date_time(b"G?\xf8\x00\x00\x00\x00\x00\x00\x89X\x03\x00\x00\x00UTC\x87"),
                "a DateTime of another form",
            ),
            (
                date_time(
                    b"\x8a\x08\x00\x00\x00\x00\x00\x00\x00\x40\x89X\x03\x00\x00\x00UTC\x87",
                ),
                "a DateTime out of the range of the model",
            ),
            (date_time(b"}"), "a DateTime of another form"),
            (
                b"\x80\x03cDateTime.DateTime\nDateTime\n)\x81.".to_vec(),
                "a DateTime without a state",
            ),
            (
                b"\x80\x03cDateTime.DateTime\nDateTime\nK\x01\x85\x81.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cm\nK\n)\x81.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cm\nK\n)\x81}b.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cbuiltins\nset\n]\x85R.".to_vec(),
                "a REDUCE of a callable that the model does not model",
            ),
            (b"\x80\x03cMissing\nValue\n.".to_vec(), "a name of Missing other than V"),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(X\x01\x00\x00\x00aNNNNNNNtR."
                    .to_vec(),
                "a Message of another form",
            ),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(X\x01\x00\x00\x00aNNNNNX\x01\x00\x00\x00ntR."
                    .to_vec(),
                "a Message of another form",
            ),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(K\x01tR.".to_vec(),
                "a Message of another form",
            ),
        ] {
            let mut heap = Heap::with_values();
            let record = [CLASS, &state].concat();
            let tree = decode_record(&record).unwrap();
            assert_eq!(heap.load(&tree), Err(Unsupported(what)), "{state:?}");
        }
    }

    #[test]
    fn a_message_pickles_its_mapping_as_a_new_dict() {
        // `(m, m, Message('b', m.domain))` with `m = Message('a', 'd', None,
        // {'n': 1})`, as ZODB 6 writes it with Python 3.13. ZODB's resolver
        // pickles it again to the same bytes.
        let mut heap = Heap::with_values();
        let state = b"\x80\x03czope.i18nmessageid.message\nMessage\nq\x01(X\x01\x00\x00\x00aq\x02\
            X\x01\x00\x00\x00dq\x03N}q\x04X\x01\x00\x00\x00nq\x05K\x01sNNNtq\x06Rq\x07h\x07h\x01\
            (X\x01\x00\x00\x00bq\x08h\x03NNNNNtq\tRq\n\x87q\x0b.";
        let (class, value) = load(&mut heap, state);
        assert_eq!(dump(&heap, class, value), [CLASS, &state[..]].concat());
    }

    #[test]
    fn an_empty_message_keeps_the_shared_empty_str() {
        // `(Message(''), '')`, as ZODB 6 writes it with Python 3.13. ZODB's
        // resolver pickles it again to the same bytes. `Message.__reduce__`
        // gives the shared empty str, so the second `''` is a memo get.
        let mut heap = Heap::with_values();
        let state = b"\x80\x03czope.i18nmessageid.message\nMessage\nq\x01(X\x00\x00\x00\x00q\x02\
            NNNNNNtq\x03Rq\x04h\x02\x86q\x05.";
        let (class, value) = load(&mut heap, state);
        let items = heap.tuple(value).unwrap().to_vec();
        assert_eq!(dump(&heap, class, value), [CLASS, &state[..]].concat());
        let args = heap.instance(items[0]).unwrap().args;
        assert_eq!(heap.tuple(args).unwrap()[0], items[1]);
    }
}
