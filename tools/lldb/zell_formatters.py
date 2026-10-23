"""LLDB data formatters for Zell programs (docs/DEBUGGER.md, stage 1).

Load them with

    (lldb) command script import /path/to/zell/tools/lldb/zell_formatters.py

or let `zell debug --lldb prog.zl` do it. They rely on the DWARF the Zell
compiler emits (compiler/src/codegen/debug.rs) and on the runtime layouts of
runtime/ABI.md:

    string      "text"                     (zell_string: len at +16, pointer to the bytes at +24)
    []T         [1, 2, 3]                  children [0], [1], ...
    map[K]V     {"a": 1, "b": 2}           children ["a"], ... (walks the table of map.c)
    error       "message"                  (wrap chains joined with ": ")
    T!          value, or error: "msg"
    data enum   Ok(value = 1)              plain enums are DWARF enumerations
    interface   app.Circle (R = 2)         children: the dynamic value (via vtable[0])
    any         same, via the type word
    func(..)    app.main$lambda1           (env 0x...) when it captures
    Task<T>     pending | done: <result>
    weak *T     -> (fields) | <freed> | nil

The command `zell-rc <expr>` decodes the reference count of the object a
value points at (unowned, biased to a thread slot, cycle-collected,
immortal..., runtime/ABI.md 3.1).

Everything is read from memory; no code runs in the debuggee, so the
formatters also work on core dumps.
"""

import re

import lldb

MAX_ITEMS = 100
MAX_STRING = 4096
CATEGORY = "zell"

# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _process(v):
    return v.GetProcess()


def _read(v, addr, n):
    if addr == 0 or n <= 0:
        return b""
    err = lldb.SBError()
    data = _process(v).ReadMemory(addr, n, err)
    if not err.Success() or data is None:
        return None
    return bytes(data)


def _u64(v, addr):
    b = _read(v, addr, 8)
    if b is None or len(b) < 8:
        return None
    return int.from_bytes(b, "little", signed=False)


def _i64(v, addr):
    b = _read(v, addr, 8)
    if b is None or len(b) < 8:
        return None
    return int.from_bytes(b, "little", signed=True)


def _cstr(v, addr, limit=256):
    if addr == 0:
        return None
    err = lldb.SBError()
    s = _process(v).ReadCStringFromMemory(addr, limit, err)
    return s if err.Success() else None


def _quote(s):
    out = ['"']
    for ch in s:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif ord(ch) < 0x20:
            out.append("\\x%02x" % ord(ch))
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def string_at(v, p):
    """The text of the zell_string at address p (NULL is "")."""
    if p == 0:
        return ""
    n = _i64(v, p + 16)
    if n is None or n < 0:
        return None
    d = _u64(v, p + 24) if n else 0  # data pointer (views share their parent's bytes)
    if n and not d:
        return None
    raw = _read(v, d, min(n, MAX_STRING)) if n else b""
    if raw is None:
        return None
    s = raw.decode("utf-8", errors="replace")
    return s + ("..." if n > MAX_STRING else "")


def _raw(v):
    nv = v.GetNonSyntheticValue()
    return nv if nv.IsValid() else v


def _handle(v):
    """The pointer inside a runtime handle `{T *ptr}` (string, error, map,
    Task, weak), as an SBValue."""
    raw = _raw(v)
    p = raw.GetChildMemberWithName("ptr")
    return p if p.IsValid() else raw


def _hptr(v):
    return _handle(v).GetValueAsUnsigned(0)


def _strip(t):
    """The type behind typedefs."""
    while t.IsTypedefType():
        t = t.GetTypedefedType()
    return t


def brief(v, depth=0):
    """A one-line rendering of any value (summary, scalar or fields)."""
    if v is None or not v.IsValid():
        return "<invalid>"
    s = v.GetSummary()
    if s:
        return s
    val = v.GetValue()
    t = _strip(v.GetType())
    if t.IsPointerType() or t.GetTypeClass() in (lldb.eTypeClassBuiltin, lldb.eTypeClassEnumeration):
        return val if val is not None else "?"
    if depth > 2:
        return "{...}"
    n = v.GetNumChildren()
    parts = []
    for i in range(min(n, 16)):
        c = v.GetChildAtIndex(i)
        name = c.GetName() or ""
        b = brief(c, depth + 1)
        parts.append(b if name.startswith("[") else "%s = %s" % (name, b))
    if n > 16:
        parts.append("...")
    if val is not None and not parts:
        return val
    return "(" + ", ".join(parts) + ")"


_BASIC = {
    "int": lldb.eBasicTypeLongLong,
    "int64": lldb.eBasicTypeLongLong,
    "uint": lldb.eBasicTypeUnsignedLongLong,
    "uint64": lldb.eBasicTypeUnsignedLongLong,
    "int32": lldb.eBasicTypeInt,
    "uint32": lldb.eBasicTypeUnsignedInt,
    "int16": lldb.eBasicTypeShort,
    "uint16": lldb.eBasicTypeUnsignedShort,
    "int8": lldb.eBasicTypeSignedChar,
    "uint8": lldb.eBasicTypeUnsignedChar,
    "byte": lldb.eBasicTypeUnsignedChar,
    "bool": lldb.eBasicTypeBool,
    "float": lldb.eBasicTypeDouble,
    "float32": lldb.eBasicTypeFloat,
    "char": lldb.eBasicTypeChar32,
}

_type_cache = {}


def find_type(v, name):
    """The SBType of Zell type `name` (as in type infos: `app.Point`,
    `*app.Point`, `[]int`, `int`), or None."""
    target = v.GetTarget()
    key = (target.GetExecutable().fullpath, name)
    if key in _type_cache:
        return _type_cache[key]
    t = None
    if name.startswith("*"):
        inner = find_type(v, name[1:])
        t = inner.GetPointerType() if inner else None
    elif name in _BASIC:
        t = target.GetBasicType(_BASIC[name])
        named = target.FindFirstType(name)
        if named.IsValid() and named.GetByteSize() == t.GetByteSize() and named.IsTypedefType():
            t = named
    else:
        found = target.FindFirstType(name)
        t = found if found.IsValid() else None
    _type_cache[key] = t
    return t


def _value_at(v, name, addr, t):
    return v.CreateValueFromAddress(name, addr, t)


def _ptr_value(v, name, p, t):
    """A value of pointer type t holding address p."""
    data = lldb.SBData.CreateDataFromUInt64Array(lldb.eByteOrderLittle, 8, [p])
    return v.CreateValueFromData(name, data, t)


# --------------------------------------------------------------------------
# string, error
# --------------------------------------------------------------------------


def string_summary(v, _dict):
    s = string_at(v, _hptr(v))
    return "<unreadable>" if s is None else _quote(s)


def error_message(v, p, depth=0):
    """The message of the zell_error at p (follows `inner` like
    zell_error_message; typed errors show their type and value)."""
    if p == 0:
        return None
    parts = []
    while p and depth < 16:
        msg_fn = _u64(v, p + 48)
        if msg_fn:
            ti = _u64(v, p + 32) or 0
            tname = _cstr(v, _u64(v, ti) or 0) if ti else None
            payload = _u64(v, p + 40) or 0
            text = tname or "error"
            t = find_type(v, tname) if tname else None
            if t is not None and payload:
                text += " " + brief(_value_at(v, "payload", payload + 16, t))
            parts.append(text)
        else:
            s = string_at(v, _u64(v, p + 16) or 0)
            parts.append(s if s is not None else "?")
        p = _u64(v, p + 24) or 0
        depth += 1
    return ": ".join(parts)


def error_summary(v, _dict):
    p = _hptr(v)
    if p == 0:
        return "nil"
    m = error_message(v, p)
    return _quote(m) if m is not None else "<unreadable>"


def char_summary(v, _dict):
    raw = _raw(v)
    if raw.GetByteSize() != 4:  # C `char` in runtime frames
        return None
    n = raw.GetValueAsUnsigned(0)
    try:
        ch = chr(n)
    except (ValueError, OverflowError):
        return "U+%04X" % n
    return "'" + _quote(ch)[1:-1].replace("'", "\\'") + "'"


# --------------------------------------------------------------------------
# slices
# --------------------------------------------------------------------------


class SliceProvider:
    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        raw = _raw(self.v)
        data = raw.GetChildMemberWithName("data")
        self.addr = data.GetValueAsUnsigned(0)
        self.len = max(0, raw.GetChildMemberWithName("len").GetValueAsSigned(0))
        self.et = data.GetType().GetPointeeType()
        self.size = self.et.GetByteSize() or 1
        return False

    def num_children(self):
        return min(self.len, 10000) if self.addr else 0

    def get_child_index(self, name):
        try:
            return int(name.lstrip("[").rstrip("]"))
        except ValueError:
            return -1

    def get_child_at_index(self, i):
        if i < 0 or i >= self.num_children():
            return None
        return _value_at(self.v, "[%d]" % i, self.addr + i * self.size, self.et)

    def has_children(self):
        return True


def slice_summary(v, _dict):
    p = SliceProvider(v, _dict)
    if p.addr == 0 and p.len == 0:
        return "[]"
    items = [brief(p.get_child_at_index(i)) for i in range(min(p.len, MAX_ITEMS))]
    if p.len > MAX_ITEMS:
        items.append("...(%d more)" % (p.len - MAX_ITEMS))
    return "[" + ", ".join(items) + "]"


# --------------------------------------------------------------------------
# maps (runtime/src/map.c: count +64, cap +72, slot_size +88, ctrl +112, slots +120)
# --------------------------------------------------------------------------


class MapProvider:
    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.entries = []
        raw = _raw(self.v)
        m = _hptr(raw)
        self.m = m
        if m == 0:
            return False
        table = _handle(raw).Dereference()
        slots = table.GetChildMemberWithName("slots")
        et = slots.GetType().GetPointeeType()
        self.kt = et.GetFieldAtIndex(0).GetType()
        self.vt = et.GetFieldAtIndex(1).GetType()
        self.voff = et.GetFieldAtIndex(1).GetOffsetInBytes()
        cap = _i64(raw, m + 72) or 0
        slot_size = _i64(raw, m + 88) or 0
        ctrl = _u64(raw, m + 112) or 0
        base = slots.GetValueAsUnsigned(0)
        if cap <= 0 or cap > (1 << 26) or not ctrl or not base:
            return False
        cb = _read(raw, ctrl, cap) or b""
        for i, c in enumerate(cb):
            if c & 0x80 == 0:
                self.entries.append(base + i * slot_size)
                if len(self.entries) >= 10000:
                    break
        return False

    def _key(self, i):
        return _value_at(self.v, "key", self.entries[i], self.kt)

    def num_children(self):
        return len(self.entries)

    def get_child_index(self, name):
        for i in range(len(self.entries)):
            if "[%s]" % brief(self._key(i)) == name:
                return i
        return -1

    def get_child_at_index(self, i):
        if i < 0 or i >= len(self.entries):
            return None
        name = "[%s]" % brief(self._key(i))
        return _value_at(self.v, name, self.entries[i] + self.voff, self.vt)

    def has_children(self):
        return True

    def pairs(self):
        out = []
        for i in range(len(self.entries)):
            out.append((brief(self._key(i)), brief(self.get_child_at_index(i))))

        def order(kv):
            k = kv[0]
            try:
                return (0, float(k), "")
            except ValueError:
                return (1, 0.0, k)

        out.sort(key=order)
        return out


def map_summary(v, _dict):
    p = MapProvider(v, _dict)
    if p.m == 0:
        return "nil"
    pairs = p.pairs()
    items = ["%s: %s" % kv for kv in pairs[:MAX_ITEMS]]
    if len(pairs) > MAX_ITEMS:
        items.append("...")
    return "{" + ", ".join(items) + "}"


# --------------------------------------------------------------------------
# T!, enums, tuples
# --------------------------------------------------------------------------


def fallible_summary(v, _dict):
    raw = _raw(v)
    if raw.GetChildMemberWithName("ptr").IsValid():  # void! is just the error
        p = _hptr(raw)
        return "ok" if p == 0 else "error: " + _quote(error_message(v, p) or "?")
    err = _hptr(raw.GetChildMemberWithName("err"))
    if err:
        return "error: " + _quote(error_message(v, err) or "?")
    return brief(raw.GetChildMemberWithName("value"))


def is_data_enum(t, _dict):
    t = _strip(t)
    if t.GetTypeClass() != lldb.eTypeClassStruct or t.GetNumberOfFields() != 2:
        return False
    a, b = t.GetFieldAtIndex(0), t.GetFieldAtIndex(1)
    return a.GetName() == "tag" and b.GetName() == "payload" and (b.GetType().GetName() or "").endswith(".payload")


def enum_summary(v, _dict):
    raw = _raw(v)
    tag = raw.GetChildMemberWithName("tag")
    name = tag.GetValue()
    if name is None:
        return "<bad tag>"
    variant = raw.GetChildMemberWithName("payload").GetChildMemberWithName(name)
    if not variant.IsValid():
        return "%s(<tag %s>)" % (raw.GetTypeName(), name)
    n = variant.GetNumChildren()
    if n == 0:
        return name
    fields = []
    for i in range(n):
        c = variant.GetChildAtIndex(i)
        fields.append("%s = %s" % (c.GetName(), brief(c)))
    return "%s(%s)" % (name, ", ".join(fields))


class EnumProvider:
    """Children of a data enum: the fields of the active variant."""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        raw = _raw(self.v)
        name = raw.GetChildMemberWithName("tag").GetValue()
        self.variant = raw.GetChildMemberWithName("payload").GetChildMemberWithName(name or "")
        return False

    def num_children(self):
        return self.variant.GetNumChildren() if self.variant.IsValid() else 0

    def get_child_index(self, name):
        for i in range(self.num_children()):
            if self.variant.GetChildAtIndex(i).GetName() == name:
                return i
        return -1

    def get_child_at_index(self, i):
        return self.variant.GetChildAtIndex(i)

    def has_children(self):
        return self.num_children() > 0


def tuple_summary(v, _dict):
    raw = _raw(v)
    return "(" + ", ".join(brief(raw.GetChildAtIndex(i)) for i in range(raw.GetNumChildren())) + ")"


# --------------------------------------------------------------------------
# interfaces and any
# --------------------------------------------------------------------------


def is_interface(t, _dict):
    t = _strip(t)
    if t.GetTypeClass() != lldb.eTypeClassStruct or t.GetNumberOfFields() != 2:
        return False
    a, b = t.GetFieldAtIndex(0), t.GetFieldAtIndex(1)
    if a.GetName() != "data":
        return False
    if b.GetName() == "vtable":
        return b.GetType().GetPointeeType().GetName() == "zell_vtable"
    return b.GetName() == "type" and t.GetName() == "any"


# Scalar types an `any` stores inline, in its data word (runtime/ABI.md 3.7).
INLINE_ANY = {"bool", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16",
              "uint32", "uint64", "byte", "rune", "float", "float32", "float64", "char"}


def dynamic_value(v):
    """(type name, SBValue or None) of an interface or `any` value, or
    (None, None) for nil."""
    raw = _raw(v)
    data = raw.GetChildMemberWithName("data").GetValueAsUnsigned(0)
    second = raw.GetChildAtIndex(1).GetValueAsUnsigned(0)
    if second == 0:
        return None, None
    if raw.GetChildAtIndex(1).GetName() == "vtable":
        ti = _u64(raw, second) or 0
        boxed = _i64(raw, second + 8)
    else:
        ti, boxed = second, 1
    name = _cstr(raw, _u64(raw, ti) or 0) if ti else None
    if not name:
        return "?", None
    t = find_type(raw, name)
    if t is None:
        return name, None
    if name.startswith("*") and not boxed:
        return name, _ptr_value(v, "value", data, t)
    if raw.GetChildAtIndex(1).GetName() != "vtable" and name in INLINE_ANY:
        # an `any` holds bool/integer/float/char values in its data word
        return name, _value_at(v, "value", raw.GetChildMemberWithName("data").GetLoadAddress(), t)
    return name, _value_at(v, "value", data, t)


def interface_summary(v, _dict):
    name, dv = dynamic_value(v)
    if name is None:
        return "nil"
    if dv is None:
        return name
    return "%s %s" % (name, brief(dv))


class InterfaceProvider:
    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        _, self.dv = dynamic_value(self.v)
        return False

    def _target(self):
        dv = self.dv
        if dv is not None and _strip(dv.GetType()).IsPointerType():
            dv = dv.Dereference()
        return dv

    def num_children(self):
        t = self._target()
        if t is None:
            return 0
        n = t.GetNumChildren()
        return n if n > 0 else 1

    def get_child_index(self, name):
        t = self._target()
        if t is None:
            return -1
        return t.GetIndexOfChildWithName(name)

    def get_child_at_index(self, i):
        t = self._target()
        if t is None:
            return None
        if t.GetNumChildren() == 0:
            return self.dv
        return t.GetChildAtIndex(i)

    def has_children(self):
        return self.dv is not None


# --------------------------------------------------------------------------
# closures, tasks, weak pointers
# --------------------------------------------------------------------------


def function_name(v, addr):
    target = v.GetTarget()
    a = target.ResolveLoadAddress(addr)
    f = a.GetFunction()
    if f.IsValid():
        return f.GetName()
    sym = a.GetSymbol()
    if not sym.IsValid():
        return "0x%x" % addr
    name = sym.GetName()
    if name.endswith("$thunk"):
        base = name[: -len("$thunk")]
        for ctx in target.FindFunctions(base):
            if ctx.GetFunction().IsValid():
                return ctx.GetFunction().GetName()
        return base
    return name


def closure_summary(v, _dict):
    raw = _raw(v)
    fn = raw.GetChildMemberWithName("fn").GetValueAsUnsigned(0)
    if fn == 0:
        return "nil"
    env = raw.GetChildMemberWithName("env").GetValueAsUnsigned(0)
    s = function_name(v, fn)
    return s + (" (env 0x%x)" % env if env else "")


# Fallback layout of struct zell_task (runtime/src/task.c, x86-64 glibc)
# for executables without the zell_debug_task_layout table (gc'd release).
_TASK_FALLBACK = (104, 216, 1)
_task_layouts = {}


def task_layout(v):
    """(state offset, slot offset, done value) from the runtime's
    `zell_debug_task_layout` table (ABI.md §20)."""
    target = v.GetTarget()
    key = target.GetExecutable().fullpath
    if key in _task_layouts:
        return _task_layouts[key]
    layout = None
    for ctx in target.FindSymbols("zell_debug_task_layout"):
        addr = ctx.GetSymbol().GetStartAddress().GetLoadAddress(target)
        if addr == lldb.LLDB_INVALID_ADDRESS:
            continue
        words = [_i64(v, addr + 8 * i) for i in range(4)]
        if None not in words and words[0] == 1:
            layout = (words[1], words[2], words[3])
            break
    if layout is None:
        return _TASK_FALLBACK
    _task_layouts[key] = layout
    return layout


def task_summary(v, _dict):
    raw = _raw(v)
    p = _hptr(raw)
    if p == 0:
        return "nil"
    state_off, slot_off, done = task_layout(v)
    b = _read(raw, p + state_off, 4)
    state = int.from_bytes(b, "little", signed=True) if b else -1
    if state != done:
        return "pending" if state == 0 else "<state %d>" % state
    m = re.match(r"^Task<(.*)>$", raw.GetTypeName() or "")
    inner = m.group(1) if m else "void"
    if inner == "void":
        return "done"
    t = find_type(v, inner)
    if t is None:
        return "done"
    return "done: " + brief(_value_at(v, "result", p + slot_off, t))


def weak_summary(v, _dict):
    raw = _raw(v)
    box = _hptr(raw)
    if box == 0:
        return "nil"
    target = _handle(raw).Dereference().GetChildMemberWithName("target")
    if target.GetValueAsUnsigned(0) == 0:
        return "<freed>"
    return "-> " + brief(target.Dereference().GetChildMemberWithName("value"))


# --------------------------------------------------------------------------
# await-aware `next`
# --------------------------------------------------------------------------


def _line_text(path, line):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for i, text in enumerate(f, 1):
                if i == line:
                    return text.split("//", 1)[0]
    except OSError:
        pass
    return ""


def _next_line(frame, line):
    """The smallest line after `line` with code in the same function."""
    fn = frame.GetFunction()
    cu = frame.GetCompileUnit()
    target = frame.GetThread().GetProcess().GetTarget()
    start = fn.GetStartAddress().GetLoadAddress(target)
    end = fn.GetEndAddress().GetLoadAddress(target)
    spec = frame.GetLineEntry().GetFileSpec()
    best = None
    for i in range(cu.GetNumLineEntries()):
        e = cu.GetLineEntryAtIndex(i)
        a = e.GetStartAddress().GetLoadAddress(target)
        if not (start <= a < end) or e.GetLine() <= line or e.GetFileSpec() != spec:
            continue
        if best is None or e.GetLine() < best:
            best = e.GetLine()
    return best


def zell_next(debugger, command, result, _dict):
    """`next` that follows `await`: when stepping over a statement with an
    `await` suspends the coroutine (lldb then stops in the caller or the
    scheduler), continue until the coroutine reaches its next line, on
    whatever worker thread resumes it."""
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    thread = process.GetSelectedThread()
    frame = thread.GetFrameAtIndex(0)
    le = frame.GetLineEntry()
    fname = frame.GetFunction().GetName()
    line = le.GetLine()
    has_await = "await" in _line_text(le.GetFileSpec().fullpath, line)
    nxt = _next_line(frame, line) if has_await else None
    if nxt is None:
        debugger.HandleCommand("thread step-over")
        return
    # Set before stepping: other threads run during the step and may resume
    # the coroutine on a worker right away.
    bp = target.BreakpointCreateByLocation(le.GetFileSpec(), nxt)
    bp.SetOneShot(True)
    debugger.HandleCommand("thread step-over")
    if not process.IsValid() or process.GetState() != lldb.eStateStopped:
        return
    for t in process:
        if t.GetStopReason() == lldb.eStopReasonBreakpoint and t.GetStopReasonDataAtIndex(0) == bp.GetID():
            process.SetSelectedThread(t)
            return
    f2 = process.GetSelectedThread().GetFrameAtIndex(0)
    if f2.GetFunction().GetName() == fname and f2.GetLineEntry().GetLine() != line:
        target.BreakpointDelete(bp.GetID())
        return
    debugger.HandleCommand("continue")



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


def zell_rc(debugger, command, result, _dict):
    """zell-rc <expr>: the reference count of the heap object <expr> points
    at (a *T points at the payload: its header is 16 bytes before; strings,
    maps, errors and tasks point at the header)."""
    target = debugger.GetSelectedTarget()
    frame = target.GetProcess().GetSelectedThread().GetSelectedFrame()
    v = frame.EvaluateExpression(command.strip())
    if not v.IsValid() or v.GetError().Fail():
        result.SetError("cannot evaluate %r" % command)
        return
    h = _raw(v).GetChildMemberWithName("ptr")
    if h.IsValid():
        addr = h.GetValueAsUnsigned(0)        # a handle: string, map, error, Task, weak
    elif v.GetType().IsPointerType():
        addr = v.GetValueAsUnsigned(0)
        addr = addr - 16 if addr else 0       # a *T: the payload
    else:
        addr = v.GetValueAsUnsigned(0)
    if addr == 0:
        result.AppendMessage("nil")
        return
    err = lldb.SBError()
    word = target.GetProcess().ReadUnsignedFromMemory(addr, 8, err)
    if err.Fail():
        result.SetError("cannot read 0x%x" % addr)
        return
    result.AppendMessage("0x%x: %s" % (addr, decode_refcount(word)))


# --------------------------------------------------------------------------
# registration
# --------------------------------------------------------------------------


def _run(debugger, cmd):
    res = lldb.SBCommandReturnObject()
    debugger.GetCommandInterpreter().HandleCommand(cmd, res)
    if not res.Succeeded():
        print("zell_formatters: `%s` failed: %s" % (cmd, res.GetError()))


def __lldb_init_module(debugger, _dict):
    m = __name__
    c = "-w " + CATEGORY
    cmds = [
        "type summary add %s -v -F %s.string_summary string" % (c, m),
        "type summary add %s -v -F %s.error_summary error" % (c, m),
        "type synthetic add %s -l %s.SliceProvider -x '^\\[\\]'" % (c, m),
        "type summary add %s -F %s.slice_summary -x '^\\[\\]'" % (c, m),
        "type synthetic add %s -l %s.MapProvider -x '^map\\['" % (c, m),
        "type summary add %s -v -F %s.map_summary -x '^map\\['" % (c, m),
        "type summary add %s -v -F %s.fallible_summary -x '!$'" % (c, m),
        "type summary add %s -F %s.tuple_summary -x '^\\(.*\\)$'" % (c, m),
        "type summary add %s -F %s.closure_summary -x '^func\\('" % (c, m),
        "type summary add %s -v -F %s.task_summary -x '^Task<'" % (c, m),
        "type summary add %s -v -F %s.weak_summary -x '^weak \\*'" % (c, m),
        "type summary add %s -F %s.enum_summary --recognizer-function %s.is_data_enum" % (c, m, m),
        "type synthetic add %s -l %s.EnumProvider --recognizer-function %s.is_data_enum" % (c, m, m),
        "type summary add %s -F %s.interface_summary --recognizer-function %s.is_interface" % (c, m, m),
        "type synthetic add %s -l %s.InterfaceProvider --recognizer-function %s.is_interface" % (c, m, m),
        "type summary add %s -v -F %s.char_summary char" % (c, m),
        "type format add %s -f d byte int8 uint8" % c,
        "type category enable %s" % CATEGORY,
    ]
    for cmd in cmds:
        _run(debugger, cmd)
    _run(debugger, "command script add -o -f %s.zell_next zell-next" % m)
    _run(debugger, "command script add -o -f %s.zell_rc zell-rc" % m)
    # A panic stops in the debugger at the panic site.
    _run(debugger, "breakpoint set -r '^zell_panic' -N ZellPanic")
