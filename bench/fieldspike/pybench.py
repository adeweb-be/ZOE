"""Python micro-benchmarks of the spike on field-addressable objects.

    python pybench.py DOCS FIELDS ROUNDS

DOCS is the docs.bin of sample.py. The script makes an object of each
record, with stand-in classes for Plone and the real DateTime, then times:

- extract from the object: getattr of each declared field, and the encode
  of the values into the typed-field slots of the prototype;
- extract from the state: __getstate__, the lookup of each field, and the
  same encode;
- the load of shape (B): the decode of the slots, an update of the state
  and __setstate__, against __setstate__ alone.

It needs persistent, zodbpickle and DateTime.
"""

import io
import json
import statistics
import struct
import sys
import sysconfig
import time

from DateTime import DateTime
from persistent import Persistent
from zodbpickle import pickle

docs_path, fields_arg, rounds = sys.argv[1], sys.argv[2], int(sys.argv[3])
DECLARED = fields_arg.split(",")


class Content(Persistent):
    pass


class Value:
    """A stand-in for a value class of Plone, such as RichTextValue."""

    def __init__(self, *args):
        self.args = args

    def __setstate__(self, state):
        self.__dict__.update(state if isinstance(state, dict) else {"state": state})


class Ref:
    def __init__(self, pid):
        self.pid = pid


class Unpickler(pickle.Unpickler):
    def find_class(self, module, name):
        if module == "DateTime.DateTime" and name == "DateTime":
            return DateTime
        if module.startswith("plone.app.contenttypes.content"):
            return Content
        if module in ("builtins", "copy_reg", "copyreg", "__builtin__"):
            return super().find_class(module, name)
        return Value

    def persistent_load(self, pid):
        return Ref(pid)


def read_groups(path):
    records = []
    with open(path, "rb") as f:
        data = f.read()
    at = 0
    while at < len(data):
        (n,) = struct.unpack_from("<I", data, at)
        at += 4 + n
        (count,) = struct.unpack_from("<I", data, at)
        at += 4
        for _ in range(count):
            (n,) = struct.unpack_from("<I", data, at)
            records.append(data[at + 4 : at + 4 + n])
            at += 4 + n
    return records


def make(record):
    u = Unpickler(io.BytesIO(record))
    u.load()  # the class pickle
    state = u.load()
    obj = Content()
    obj.__setstate__(state)
    return obj, state


NONE, BOOL, INT, FLOAT, STR, BYTES, DATETIME, STRS, PICKLE = range(9)
HEAD = struct.Struct("<HBI")


def _str(fid, v):
    b = v.encode("utf-8", "surrogatepass")
    return HEAD.pack(fid, STR, len(b)) + b


def _int(fid, v):
    return HEAD.pack(fid, INT, 8) + struct.pack("<q", v)


def _bool(fid, v):
    return HEAD.pack(fid, BOOL, 1) + (b"\x01" if v else b"\x00")


def _float(fid, v):
    return HEAD.pack(fid, FLOAT, 8) + struct.pack("<d", v)


def _none(fid, v):
    return HEAD.pack(fid, NONE, 0)


def _bytes(fid, v):
    return HEAD.pack(fid, BYTES, len(v)) + v


def _datetime(fid, v):
    tz = (v._tz or "").encode()
    return (
        HEAD.pack(fid, DATETIME, 9 + len(tz))
        + struct.pack("<q?", v._micros, v._timezone_naive)
        + tz
    )


def _strs(fid, v):
    parts = [struct.pack("<H", len(v))]
    for i in v:
        if type(i) is not str:
            return _pickle(fid, v)
        b = i.encode("utf-8", "surrogatepass")
        parts.append(struct.pack("<I", len(b)) + b)
    body = b"".join(parts)
    return HEAD.pack(fid, STRS, len(body)) + body


def _pickle(fid, v):
    body = pickle.dumps(v, 3)
    return HEAD.pack(fid, PICKLE, len(body)) + body


ENCODERS = {
    str: _str,
    int: _int,
    bool: _bool,
    float: _float,
    type(None): _none,
    bytes: _bytes,
    DateTime: _datetime,
    tuple: _strs,
    list: _strs,
}


def encode(values):
    out = [struct.pack("<H", len(values))]
    for fid, v in values:
        out.append(ENCODERS.get(type(v), _pickle)(fid, v))
    return b"".join(out)


def decode(slots):
    (count,) = struct.unpack_from("<H", slots, 0)
    at = 2
    values = {}
    for _ in range(count):
        fid, kind, n = HEAD.unpack_from(slots, at)
        at += 7
        body = slots[at : at + n]
        at += n
        name = DECLARED[fid]
        if kind == STR:
            values[name] = body.decode("utf-8", "surrogatepass")
        elif kind == INT:
            values[name] = struct.unpack("<q", body)[0]
        elif kind == DATETIME:
            micros, naive = struct.unpack_from("<q?", body, 0)
            dt = DateTime.__new__(DateTime)
            dt.__setstate__((micros, naive, body[9:].decode() or None))
            values[name] = dt
        elif kind == STRS:
            (m,) = struct.unpack_from("<H", body, 0)
            p = 2
            items = []
            for _ in range(m):
                (k,) = struct.unpack_from("<I", body, p)
                items.append(body[p + 4 : p + 4 + k].decode("utf-8", "surrogatepass"))
                p += 4 + k
            values[name] = tuple(items)
        elif kind == BOOL:
            values[name] = body == b"\x01"
        elif kind == NONE:
            values[name] = None
        elif kind == FLOAT:
            values[name] = struct.unpack("<d", body)[0]
        elif kind == BYTES:
            values[name] = bytes(body)
        else:
            values[name] = pickle.loads(body)
    return values


MISSING = object()


def from_object(obj):
    values = []
    for fid, name in enumerate(DECLARED):
        v = getattr(obj, name, MISSING)
        if v is not MISSING:
            values.append((fid, v))
    return encode(values)


def from_state(obj):
    state = obj.__getstate__()
    values = []
    for fid, name in enumerate(DECLARED):
        v = state.get(name, MISSING)
        if v is not MISSING:
            values.append((fid, v))
    return encode(values)


def timed(items, f):
    samples = []
    for _ in range(rounds):
        start = time.perf_counter_ns()
        for item in items:
            f(item)
        samples.append((time.perf_counter_ns() - start) / len(items))
    return statistics.median(samples)


records = read_groups(docs_path)
objects = [make(r) for r in records]
objs = [o for o, _ in objects]
slots = [from_object(o) for o in objs]
assert all(from_state(o) == s for o, s in zip(objs, slots))
states = [o.__getstate__() for o in objs]
fresh = [Content() for _ in objs]


def load_today(i):
    u = Unpickler(io.BytesIO(records[i]))
    u.load()
    state = u.load()
    fresh[i].__setstate__(state)


def load_b(i):
    u = Unpickler(io.BytesIO(records[i]))
    u.load()
    state = u.load()
    state.update(decode(slots[i]))
    fresh[i].__setstate__(state)


def decode_only(i):
    decode(slots[i])


def encode_only(i):
    encode(values[i])


values = []
for o in objs:
    values.append(
        [(fid, getattr(o, n)) for fid, n in enumerate(DECLARED) if hasattr(o, n)]
    )

idx = list(range(len(objs)))
gil = getattr(sys, "_is_gil_enabled", lambda: True)()
result = {
    "python": sys.version.split()[0],
    "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
    "gil_enabled": gil,
    "objects": len(objs),
    "slot_bytes_mean": sum(map(len, slots)) / len(slots),
    "extract_from_object_ns": timed(objs, from_object),
    "extract_from_state_ns": timed(objs, from_state),
    "encode_only_ns": timed(idx, encode_only),
    "decode_slots_ns": timed(idx, decode_only),
    "load_today_ns": timed(idx, load_today),
    "load_b_ns": timed(idx, load_b),
}
print(json.dumps(result))
