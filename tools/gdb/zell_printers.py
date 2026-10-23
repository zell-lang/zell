"""GDB pretty-printers for Zell programs (docs/DEBUGGER.md, stage 1).

    (gdb) source /path/to/zell/tools/gdb/zell_printers.py

or `zell debug --gdb prog.zl`. Same coverage as tools/lldb/zell_formatters.py:
string, []T, map[K]V, error, T!, data enums, interfaces/any, func values,
Task<T>, weak *T and char. They only read memory (core dumps work too).
"""

import re

import gdb

MAX_ITEMS = 100
MAX_STRING = 4096


def _inferior():
    return gdb.selected_inferior()


def _read(addr, n):
    if addr == 0 or n <= 0:
        return b""
    try:
        return bytes(_inferior().read_memory(addr, n))
    except gdb.MemoryError:
        return None


def _i64(addr):
    b = _read(addr, 8)
    return int.from_bytes(b, "little", signed=True) if b and len(b) == 8 else None


def _u64(addr):
    b = _read(addr, 8)
    return int.from_bytes(b, "little") if b and len(b) == 8 else None


def _cstr(addr, limit=256):
    if not addr:
        return None
    b = _read(addr, limit) or b""
    return b.split(b"\0", 1)[0].decode("utf-8", errors="replace")


def _quote(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t") + '"'


def _addr(v):
    """The address in a pointer or in a runtime handle `{T *ptr}`."""
    t = v.type.strip_typedefs()
    if t.code == gdb.TYPE_CODE_STRUCT and [f.name for f in t.fields()] == ["ptr"]:
        v = v["ptr"]
        t = v.type.strip_typedefs()
    return int(v.cast(gdb.lookup_type("unsigned long"))) if t.code == gdb.TYPE_CODE_PTR else int(v)


def _deref(v):
    """`*v` for a pointer or a runtime handle."""
    t = v.type.strip_typedefs()
    if t.code == gdb.TYPE_CODE_STRUCT and [f.name for f in t.fields()] == ["ptr"]:
        v = v["ptr"]
    return v.dereference()


def string_at(p):
    if p == 0:
        return ""
    n = _i64(p + 16)
    if n is None or n < 0:
        return None
    d = _u64(p + 24) if n else 0  # data pointer (views share their parent's bytes)
    if n and not d:
        return None
    raw = _read(d, min(n, MAX_STRING)) if n else b""
    if raw is None:
        return None
    return raw.decode("utf-8", errors="replace") + ("..." if n > MAX_STRING else "")


def error_message(p):
    parts = []
    depth = 0
    while p and depth < 16:
        if _u64(p + 48):
            ti = _u64(p + 32) or 0
            parts.append(_cstr(_u64(ti) or 0) or "error")
        else:
            s = string_at(_u64(p + 16) or 0)
            parts.append(s if s is not None else "?")
        p = _u64(p + 24) or 0
        depth += 1
    return ": ".join(parts)


def brief(v):
    """One-line text of a value, using these printers where they apply."""
    pp = lookup(v)
    if pp is not None:
        s = pp.to_string()
        if hasattr(pp, "children") and getattr(pp, "display_hint", lambda: None)() == "array":
            items = [brief(c) for _, c in pp.children()]
            return "[" + ", ".join(items) + "]"
        return str(s)
    t = v.type.strip_typedefs()
    if t.code == gdb.TYPE_CODE_STRUCT:
        return "(" + ", ".join("%s = %s" % (f.name, brief(v[f.name])) for f in t.fields()) + ")"
    return str(v)


_BASIC = {"int": "long", "uint": "unsigned long", "int32": "int", "uint32": "unsigned int", "int16": "short",
          "uint16": "unsigned short", "int8": "signed char", "uint8": "unsigned char", "byte": "unsigned char",
          "float": "double", "float32": "float", "bool": "bool"}


def find_type(name):
    if name.startswith("*"):
        t = find_type(name[1:])
        return t.pointer() if t is not None else None
    for cand in (name, _BASIC.get(name)):
        if not cand:
            continue
        for prefix in ("", "struct "):
            try:
                return gdb.lookup_type(prefix + cand)
            except gdb.error:
                pass
    return None


class StringPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        s = string_at(_addr(self.v))
        return "<unreadable>" if s is None else _quote(s)


class ErrorPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        p = _addr(self.v)
        return "nil" if p == 0 else _quote(error_message(p))


class CharPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        n = int(self.v)
        try:
            return "'%s'" % chr(n)
        except (ValueError, OverflowError):
            return "U+%04X" % n


class SlicePrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        return "%s len %d" % (self.v.type.name or "[]", int(self.v["len"]))

    def display_hint(self):
        return "array"

    def children(self):
        data = self.v["data"]
        n = min(max(int(self.v["len"]), 0), MAX_ITEMS)
        if int(data.cast(gdb.lookup_type("unsigned long"))) == 0:
            return
        for i in range(n):
            yield "[%d]" % i, (data + i).dereference()


class MapPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        if _addr(self.v) == 0:
            return "nil"
        return "%s len %d" % (self.v.type.name, int(_deref(self.v)["count"]))

    def display_hint(self):
        return "map"

    def children(self):
        if _addr(self.v) == 0:
            return
        table = _deref(self.v)
        cap = int(table["cap"])
        slots = table["slots"]
        ctrl = _read(int(table["ctrl"].cast(gdb.lookup_type("unsigned long"))), cap) or b""
        n = 0
        for i, c in enumerate(ctrl):
            if c & 0x80:
                continue
            e = (slots + i).dereference()
            yield "k%d" % n, e["key"]
            yield "v%d" % n, e["value"]
            n += 1
            if n >= MAX_ITEMS:
                break


class FalliblePrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        t = self.v.type.strip_typedefs()
        if [f.name for f in _fields(t)] == ["ptr"]:
            p = _addr(self.v)
            return "ok" if p == 0 else "error: " + _quote(error_message(p))
        err = _addr(self.v["err"])
        if err:
            return "error: " + _quote(error_message(err))
        return brief(self.v["value"])


class EnumPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        tag = self.v["tag"]
        name = str(tag)
        try:
            variant = self.v["payload"][name]
        except gdb.error:
            return "<tag %s>" % name
        fields = variant.type.strip_typedefs().fields()
        if not fields:
            return name
        return "%s(%s)" % (name, ", ".join("%s = %s" % (f.name, brief(variant[f.name])) for f in fields))


# Scalar types an `any` stores inline, in its data word (runtime/ABI.md 3.7).
INLINE_ANY = {"bool", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16",
              "uint32", "uint64", "byte", "rune", "float", "float32", "float64", "char"}


class InterfacePrinter:
    def __init__(self, v):
        self.v = v

    def dynamic(self):
        data = _addr(self.v["data"])
        t = self.v.type.strip_typedefs()
        second = t.fields()[1].name
        w = _addr(self.v[second])
        if w == 0:
            return None, None
        if second == "vtable":
            ti, boxed = _u64(w) or 0, _i64(w + 8)
        else:
            ti, boxed = w, 1
        name = _cstr(_u64(ti) or 0) if ti else None
        if not name:
            return "?", None
        typ = find_type(name)
        if typ is None:
            return name, None
        if name.startswith("*") and not boxed:
            return name, gdb.Value(data).cast(typ)
        if second != "vtable" and name in INLINE_ANY:
            # an `any` holds bool/integer/float/char values in its data word
            return name, self.v["data"].address.cast(typ.pointer()).dereference()
        return name, gdb.Value(data).cast(typ.pointer()).dereference()

    def to_string(self):
        name, dv = self.dynamic()
        if name is None:
            return "nil"
        return name if dv is None else "%s %s" % (name, brief(dv))


class ClosurePrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        fn = _addr(self.v["fn"])
        if fn == 0:
            return "nil"
        env = _addr(self.v["env"])
        try:
            name = gdb.block_for_pc(fn).function.name
        except (RuntimeError, AttributeError):
            out = gdb.execute("info symbol 0x%x" % fn, to_string=True).split(" ")[0]
            name = out[: -len("$thunk")] if out.endswith("$thunk") else out
        return name + (" (env 0x%x)" % env if env else "")


def _task_layout():
    try:
        tbl = gdb.parse_and_eval("(long *)&zell_debug_task_layout")
        if int(tbl[0]) == 1:
            return int(tbl[1]), int(tbl[2]), int(tbl[3])
    except gdb.error:
        pass
    return 104, 216, 1


class TaskPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        p = _addr(self.v)
        if p == 0:
            return "nil"
        state_off, slot_off, done = _task_layout()
        b = _read(p + state_off, 4)
        state = int.from_bytes(b, "little") if b else -1
        if state != done:
            return "pending" if state == 0 else "<state %d>" % state
        m = re.match(r"^Task<(.*)>$", self.v.type.name or "")
        inner = m.group(1) if m else "void"
        t = find_type(inner) if inner != "void" else None
        if t is None:
            return "done"
        return "done: " + brief(gdb.Value(p + slot_off).cast(t.pointer()).dereference())


class WeakPrinter:
    def __init__(self, v):
        self.v = v

    def to_string(self):
        if _addr(self.v) == 0:
            return "nil"
        target = _deref(self.v)["target"]
        if _addr(target) == 0:
            return "<freed>"
        return "-> " + brief(target.dereference()["value"])


def _fields(t):
    try:
        return t.fields()
    except TypeError:
        return []


def lookup(v):
    t = v.type
    name = t.name or (t.tag if t.code == gdb.TYPE_CODE_STRUCT else None) or ""
    if name:
        if name == "string":
            return StringPrinter(v)
        if name == "error":
            return ErrorPrinter(v)
        if name == "char" and t.strip_typedefs().sizeof == 4:
            return CharPrinter(v)
        if name.startswith("map["):
            return MapPrinter(v)
        if name.startswith("Task<"):
            return TaskPrinter(v)
        if name.startswith("weak *"):
            return WeakPrinter(v)
        if name.endswith("!"):
            return FalliblePrinter(v)
    s = t.strip_typedefs()
    if s.code != gdb.TYPE_CODE_STRUCT:
        return None
    tag = s.tag or s.name or ""
    if tag.startswith("[]"):
        return SlicePrinter(v)
    if tag.endswith("!"):
        return FalliblePrinter(v)
    if tag.startswith("func("):
        return ClosurePrinter(v)
    fs = _fields(s)
    if len(fs) == 2:
        a, b = fs[0].name, fs[1].name
        if a == "tag" and b == "payload":
            return EnumPrinter(v)
        if a == "data" and (b == "vtable" or (b == "type" and tag == "any")):
            return InterfacePrinter(v)
    return None


# --------------------------------------------------------------------------
# reference counts (runtime/ABI.md 3.1: the header word at +0)
# --------------------------------------------------------------------------

RC_WEAK = 1 << 62
RC_CYCLIC = 1 << 61
RC_BIASED = 1 << 60
RC_QUEUED = 1 << 59
RC_COUNT_MASK = (1 << 57) - 1


def decode_refcount(word):
    """The meaning of an object header's refcount word, as text: immortal,
    arena, a string view's block, an unowned count, a biased count (owner
    slot, B + S) or a cycle-collected count with its flags."""
    w = word & 0xFFFFFFFFFFFFFFFF
    if w >= 1 << 63:
        s = w - (1 << 64)
        if s == -1:
            return "immortal"
        if s == -2:
            return "arena"
        return "view of block 0x%x" % (w & ~(1 << 63))
    flags = []
    if w & RC_WEAK:
        flags.append("weak box")
    if w & RC_CYCLIC:
        names = [(60, "buffered"), (59, "marked"), (58, "dirty"), (57, "zombie")]
        flags += [n for b, n in names if w & (1 << b)]
        text = "count %d (cycle-collected)" % (w & RC_COUNT_MASK)
    elif w & RC_BIASED:
        b = w & 0xFFFF
        s = ((w >> 16) & 0x7FFFFFFF) - (1 << 30)
        slot = (w >> 47) & 0xFFF
        if w & RC_QUEUED:
            flags.append("queued for a merge")
        text = "count %d (biased to thread slot %d: owner %d, shared %d)" % (b + s, slot, b, s)
    else:
        if w & RC_QUEUED:
            flags.append("never biased")
        text = "count %d (unowned)" % (w & RC_COUNT_MASK)
    return text + ("" if not flags else ", " + ", ".join(flags))


class ZellRc(gdb.Command):
    """zell-rc EXPR: the reference count of the heap object EXPR points at
    (a *T points at the payload, 16 bytes after the header)."""

    def __init__(self):
        super().__init__("zell-rc", gdb.COMMAND_DATA)

    def invoke(self, arg, from_tty):
        v = gdb.parse_and_eval(arg)
        t = v.type.strip_typedefs()
        if t.code == gdb.TYPE_CODE_STRUCT and [f.name for f in t.fields()] == ["ptr"]:
            addr = _addr(v)                  # a handle: string, map, error, Task, weak
        elif t.code == gdb.TYPE_CODE_PTR:
            addr = _addr(v)
            addr = addr - 16 if addr else 0  # a *T: the payload
        else:
            addr = int(v)
        if addr <= 0:
            print("nil")
            return
        print("0x%x: %s" % (addr, decode_refcount(_u64(addr))))


ZellRc()


def register(objfile=None):
    target = objfile.pretty_printers if objfile is not None else gdb.pretty_printers
    if lookup not in target:
        target.append(lookup)


register()
