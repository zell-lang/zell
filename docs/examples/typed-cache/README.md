# typed-cache: generics and interfaces

A fixed-size cache, written once for every key and value type, with the
eviction policy as a pluggable interface. It is used three ways: a small
trace that shows which key each policy evicts, a typed cache of users in
front of a loader that can fail, and a benchmark-like loop of 20 000
skewed accesses that compares the two policies.

What it shows:

- a generic struct `Cache<K: Comparable, V>` and why `K` needs the
  constraint (it is a map key) while `V` does not;
- an interface (`Policy`) with two implementations, `LRU` and `LFU`,
  chosen at run time and stored in a field;
- a generic `Maybe<T>` lookup result with `Or` and `Get() T!`, the
  bridge from "not found" to the error world of `?` and `try`;
- a stricter constraint on one function (`SortedKeys<K: Ordered, V>`),
  checked where it is called;
- an iterator: `Iter()` and `Next() (K, V, bool)`, so a cache works in
  `for k, v in c`, and a type that is its own iterator (`Accesses`);
- method sets: `*Cache<K, V>` satisfies `Measured` for every `K` and `V`;
- table-driven tests with subtests.

## Run it

```sh
cd docs/examples/typed-cache
zell run
zell test
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `cache`) |
| `src/policy.zl` | `cache.policy`: the `Policy` interface, `LRU`, `LFU` |
| `src/cache.zl` | `cache`: `Maybe<T>`, `Cache<K, V>`, its iterator, two generic helpers |
| `src/main.zl` | the trace, the typed cache of users, the comparison loop |
| `src/policy_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "typed-cache"
version = "0.1.0"
namespace = "cache"
entry = "src/main.zl"
```

## src/policy.zl: eviction as an interface

Interfaces in Zell cannot be generic (yet), so a policy cannot be a
`Policy<K>`. The design here makes that a non-issue: the cache keeps its
entries in numbered slots and tells the policy only slot numbers, so one
policy type serves every `Cache<K, V>` and never copies a key.

An interface is satisfied structurally: `LRU` and `LFU` do not declare
that they implement `Policy`, they just have the methods. Both keep a
clock and stamp each use; `LFU` adds a use count and breaks ties by the
stamp, so among equally used slots the one used least recently goes
first. `oldest` scans a map; map order is unspecified, but the stamps
are unique, so the answer is not.

<!-- file: src/policy.zl -->
```zell title="src/policy.zl"
// Eviction policies. The cache stores entries in numbered slots and
// tells its policy what happens to each slot; the policy decides which
// slot to empty when the cache is full. Policies see only slot numbers,
// never keys or values, so one policy type serves every Cache<K, V>.
namespace cache.policy

// Policy is an ordinary interface: any struct with these methods is one.
interface Policy {
    func Name() string
    func Inserted(slot int)
    func Used(slot int)
    func Removed(slot int)
    // Victim is the slot to evict; called only when every slot is full.
    func Victim() int
}

// LRU evicts the slot used least recently. It stamps every use with a
// clock; the victim has the oldest stamp.
struct LRU {
    clock int
    stamp map[int]int

    static func New() *LRU {
        return new LRU{stamp: map[int]int{}}
    }

    func Name() string {
        return "LRU"
    }

    func Inserted(slot int) {
        this.Used(slot)
    }

    func Used(slot int) {
        this.clock += 1
        this.stamp[slot] = this.clock
    }

    func Removed(slot int) {
        delete(this.stamp, slot)
    }

    func Victim() int {
        return oldest(this.stamp, map[int]int{})
    }
}

// LFU evicts the slot used least often; among equally used slots, the
// one used least recently (so a burst of new keys cannot pin old ones).
struct LFU {
    clock int
    uses  map[int]int
    stamp map[int]int

    static func New() *LFU {
        return new LFU{uses: map[int]int{}, stamp: map[int]int{}}
    }

    func Name() string {
        return "LFU"
    }

    func Inserted(slot int) {
        this.uses[slot] = 0
        this.Used(slot)
    }

    func Used(slot int) {
        this.clock += 1
        this.uses[slot] += 1
        this.stamp[slot] = this.clock
    }

    func Removed(slot int) {
        delete(this.uses, slot)
        delete(this.stamp, slot)
    }

    func Victim() int {
        return oldest(this.stamp, this.uses)
    }
}

// oldest returns the slot with the smallest (rank, stamp) pair, where a
// slot's rank is ranks[slot] (0 when absent). O(n) is fine for a few
// hundred slots; a real cache would keep a list or a heap.
func oldest(stamp map[int]int, ranks map[int]int) int {
    var best = -1
    for slot, s in stamp {
        if best < 0 {
            best = slot
            continue
        }
        var (r, br) = (ranks[slot], ranks[best])
        if r < br || (r == br && s < stamp[best]) {
            best = slot
        }
    }
    return best
}
```

## src/cache.zl: the generic cache

`Cache<K: Comparable, V>` stores keys and values in parallel slices
(one slot per entry), with `index` from key to slot and `free` as the
stack of empty slots. `var k K` is the zero value of whatever `K` is,
used to clear a slot so that it does not keep a value alive.

The constraint is on the struct because `index map[K]int` needs `K` to
be comparable. `SortedKeys` needs more, `Ordered`, to sort; it is a
separate function with its own constraint, so a cache keyed by a struct
works everywhere except in `SortedKeys`, and calling it there is a
compile error at the call.

`Maybe<T>` is a small generic struct rather than a built-in: it has
`Ok()`, `Or(def)`, and `Get() T!`, which returns `ErrMissing` when there
is no value. `GetOrLoad` shows both worlds meeting: a miss calls the
loader, whose error is wrapped with `?("load")` and returned, and a
failure is not cached.

`Iter()` is what makes `for k, v in c` work: the loop calls it once and
then calls `Next()` on what it returned until the last result is
`false`. The iterator copies the three slices (they share the cache's
buffers), not a pointer to the cache.

`HitRate` takes the interface `Measured`. Any type with
`Counts() (int, int, int)` is one, so it accepts a `*Cache<string, User>`
and a `*Cache<int, string>` alike, with one compiled function.

<!-- file: src/cache.zl -->
```zell title="src/cache.zl"
// A generic cache with a fixed number of slots and a pluggable eviction
// policy, a small Result-like helper for lookups, and an iterator so a
// cache can be walked with `for k, v in c`.
namespace cache

import cache.policy

const ErrMissing = error("not in the cache")

// Maybe is the result of a lookup: a value or nothing. It is generic,
// so a Cache<string, User> returns Maybe<User> and no cast is needed.
struct Maybe<T> {
    value T
    ok    bool

    func Ok() bool {
        return this.ok
    }

    // Or returns the value, or def when there is none.
    func Or(def T) T {
        if this.ok {
            return this.value
        }
        return def
    }

    // Get turns the lookup into a T!, so it composes with ? and try.
    func Get() T! {
        if !this.ok {
            return ErrMissing
        }
        return this.value
    }
}

func Some<T>(v T) Maybe<T> {
    return Maybe<T>{value: v, ok: true}
}

func None<T>() Maybe<T> {
    return Maybe<T>{}
}

// Measured is what a report needs from anything that counts hits. Cache
// has these methods, so *Cache<K, V> satisfies it for every K and V.
interface Measured {
    func Counts() (int, int, int)
}

// Cache maps keys to values in a fixed number of slots. K must be
// Comparable because it is a map key; V can be anything.
struct Cache<K: Comparable, V> {
    keys      []K
    vals      []V
    used      []bool
    index     map[K]int
    free      []int
    policy    Policy
    hits      int
    misses    int
    evictions int

    static func New(capacity int, p Policy) *Cache<K, V> {
        var c = new Cache<K, V>{index: map[K]int{}, policy: p}
        for i in 0..capacity {
            var k K
            var v V
            c.keys = append(c.keys, k)
            c.vals = append(c.vals, v)
            c.used = append(c.used, false)
            c.free = append(c.free, capacity - 1 - i)
        }
        return c
    }

    func Len() int {
        return len(this.index)
    }

    func Get(k K) Maybe<V> {
        var (slot, ok) = this.index[k]
        if !ok {
            this.misses += 1
            return None<V>()
        }
        this.hits += 1
        this.policy.Used(slot)
        return Some(this.vals[slot])
    }

    // Put stores v under k and returns the key it evicted, if any.
    func Put(k K, v V) Maybe<K> {
        var (slot, ok) = this.index[k]
        if ok {
            this.vals[slot] = v
            this.policy.Used(slot)
            return None<K>()
        }
        var evicted = None<K>()
        if len(this.free) == 0 {
            var victim = this.policy.Victim()
            evicted = Some(this.keys[victim])
            this.remove(victim)
            this.evictions += 1
        }
        slot = this.free[len(this.free) - 1]
        this.free = this.free[..len(this.free) - 1]
        this.keys[slot] = k
        this.vals[slot] = v
        this.used[slot] = true
        this.index[k] = slot
        this.policy.Inserted(slot)
        return evicted
    }

    // GetOrLoad returns the cached value or calls load and caches what it
    // returns. A failing load is not cached and its error is returned.
    func GetOrLoad(k K, load func(K) V!) V! {
        var m = this.Get(k)
        if m.Ok() {
            return m.Get()?
        }
        var v = load(k)?("load")
        _ = this.Put(k, v)
        return v
    }

    func remove(slot int) {
        delete(this.index, this.keys[slot])
        var k K
        var v V
        this.keys[slot] = k
        this.vals[slot] = v
        this.used[slot] = false
        this.free = append(this.free, slot)
        this.policy.Removed(slot)
    }

    func Counts() (int, int, int) {
        return (this.hits, this.misses, this.evictions)
    }

    // Iter makes the cache iterable: `for k, v in c` walks the occupied
    // slots in slot order. The iterator shares the slices, not the cache.
    func Iter() Entries<K, V> {
        return Entries<K, V>{keys: this.keys, vals: this.vals, used: this.used}
    }
}

// Entries is the iterator. A Next() returning (K, V, bool) is what a
// two-variable `for` loop calls before every step; false ends the loop.
struct Entries<K, V> {
    keys []K
    vals []V
    used []bool
    i    int

    func Next() (K, V, bool) {
        for this.i < len(this.keys) {
            var i = this.i
            this.i += 1
            if this.used[i] {
                return (this.keys[i], this.vals[i], true)
            }
        }
        var k K
        var v V
        return (k, v, false)
    }
}

// SortedKeys works for any cache whose keys are Ordered: a stricter
// constraint than the cache itself needs, checked where it is called.
func SortedKeys<K: Ordered, V>(c *Cache<K, V>) []K {
    var out []K = []
    for k, _ in c {
        out = append(out, k)
    }
    slices.Sort(out)
    return out
}

// HitRate takes the interface, so it accepts every cache instance.
func HitRate(m Measured) string {
    var (hits, misses, _) = m.Counts()
    if hits + misses == 0 {
        return "n/a"
    }
    return fmt.Sprintf("%.1f%%", float(hits) * 100.0 / float(hits + misses))
}
```

## src/main.zl: three uses

The trace runs the same eight accesses through two caches of three
slots. Both evict `b` first and `c` second; at `e` they differ: `a` was
used three times but not recently, so LRU drops it and LFU keeps it.

`Accesses` is its own iterator (it has `Next()` and no `Iter()`), so
`for k in (Accesses{...})` walks a fixed pseudo-random stream: 80% of
the accesses hit 20 hot keys, 20% are spread over 500 cold ones. On
that kind of load LFU keeps the hot keys a little better.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// typed-cache: one generic cache, two eviction policies, three uses.
namespace cache

import io
import cache.policy

struct User {
    Name  string
    Admin bool
}

// trace runs the same accesses against a cache and prints what happened.
func trace(c *Cache<string, int>, name string) {
    var keys = ["a", "b", "c", "a", "a", "d", "b", "e"]
    var log []string = []
    for i, k in keys {
        if !c.Get(k).Ok() {
            var gone = c.Put(k, i)
            if gone.Ok() {
                log = append(log, k + " evicts " + gone.Or(""))
            }
        }
    }
    println(fmt.Format("  {0}: {1}; keeps {2}", name, strings.Join(log, ", "), SortedKeys(c)))
}

// Accesses is a deterministic stream of keys: 80% from 20 hot keys,
// 20% from 500 cold ones (a linear congruential generator, fixed seed).
struct Accesses {
    seed int
    left int

    func Next() (int, bool) {
        if this.left == 0 {
            return (0, false)
        }
        this.left -= 1
        this.seed = (this.seed * 1103515245 + 12345) % 2147483648
        var r = this.seed / 65536
        if r % 10 < 8 {
            return (r % 20, true)
        }
        return (20 + r % 500, true)
    }
}

func run(p Policy) {
    var c = Cache<int, string>.New(32, p)
    for k in (Accesses{seed: 42, left: 20000}) {
        if !c.Get(k).Ok() {
            _ = c.Put(k, "value " + strconv.Itoa(k))
        }
    }
    var (hits, misses, evictions) = c.Counts()
    println(fmt.Sprintf("  %s: %d hits, %d misses, %d evictions, hit rate %s", p.Name(), hits, misses, evictions, HitRate(c)))
}

func main() {
    println("eviction order, capacity 3:")
    trace(Cache<string, int>.New(3, LRU.New()), "LRU")
    trace(Cache<string, int>.New(3, LFU.New()), "LFU")

    println("a typed cache in front of a loader:")
    var users = Cache<string, User>.New(2, LRU.New())
    var loads = 0
    var load = func(name string) User! {
        loads += 1
        if name == "mallory" {
            return error("no user " + strconv.Quote(name))
        }
        return User{Name: name, Admin: name == "root"}
    }
    for name in ["root", "ann", "root", "mallory", "ann"] {
        var u = try users.GetOrLoad(name, load) catch err {
            println("  " + name + ":", err)
            continue
        }
        println("  " + name + ": admin", u.Admin)
    }
    println("  loads:", loads, "cached:", users.Len(), "hit rate:", HitRate(users))
    for name, u in users {
        println("  in cache:", name, u.Admin)
    }

    println("20000 skewed accesses, capacity 32:")
    run(LRU.New())
    run(LFU.New())

    // Maybe.Get turns "not found" into an error for code that wants one.
    var bob = try users.Get("bob").Get() catch err {
        println("bob:", err)
        return
    }
    println("bob is cached:", bob.Name)
}
```

## src/policy_test.zl: tests

The policies are tested alone (slot numbers in, a victim out), and then
through a cache with a table of cases: the same accesses, a different
policy, a different victim. `t.Run` gives each row its own name
(`TestCacheEvictsWhatThePolicySays/lru`).

<!-- file: src/policy_test.zl -->
```zell title="src/policy_test.zl"
// Tests for the policies, alone and through a cache. A table drives the
// cache tests: the same accesses, a different policy, a different victim.
namespace cache.tests

import cache
import cache.policy

@test func TestLRUPicksTheLeastRecentlyUsed(t *testing.T) {
    var p = LRU.New()
    p.Inserted(0)
    p.Inserted(1)
    p.Inserted(2)
    p.Used(0)
    testing.Equal(t, p.Victim(), 1)
    p.Removed(1)
    testing.Equal(t, p.Victim(), 2)
}

@test func TestLFUPicksTheLeastUsedThenTheOldest(t *testing.T) {
    var p = LFU.New()
    p.Inserted(0)
    p.Inserted(1)
    p.Inserted(2)
    p.Used(0)
    p.Used(2)
    testing.Equal(t, p.Victim(), 1)
    p.Used(1)
    // Every slot has two uses now: the oldest last use loses.
    testing.Equal(t, p.Victim(), 0)
}

struct Case {
    Name   string
    Policy Policy
    Victim string
}

@test func TestCacheEvictsWhatThePolicySays(t *testing.T) {
    var cases = [
        Case{Name: "lru", Policy: LRU.New(), Victim: "x"},
        Case{Name: "lfu", Policy: LFU.New(), Victim: "z"},
    ]
    for c in cases {
        t.Run(c.Name, func(t *testing.T) {
            var cache = Cache<string, int>.New(3, c.Policy)
            _ = cache.Put("x", 1)
            _ = cache.Put("y", 2)
            _ = cache.Put("z", 3)
            for k in ["x", "x", "y", "y", "z"] {
                _ = cache.Get(k)
            }
            // LRU: x was used least recently (y and z came after it).
            // LFU: z was used least often (twice, against three times).
            var gone = cache.Put("w", 4)
            testing.Equal(t, gone.Or("-"), c.Victim)
            testing.Equal(t, cache.Len(), 3)
        })
    }
}

@test func TestMaybe(t *testing.T) {
    testing.Equal(t, Some(5).Or(0), 5)
    testing.Equal(t, None<int>().Or(7), 7)
    try None<string>().Get() catch err {
        testing.True(t, errors.Is(err, ErrMissing), "ErrMissing expected")
        return
    }
    t.Fatal("None.Get succeeded")
}

@test func TestGetOrLoadDoesNotCacheFailures(t *testing.T) {
    var c = Cache<int, string>.New(2, LRU.New())
    var calls = 0
    var load = func(k int) string! {
        calls += 1
        return error("down")
    }
    for _ in 0..2 {
        try c.GetOrLoad(1, load) catch err {
            testing.Contains(t, err.Message(), "load: down")
        }
    }
    testing.Equal(t, calls, 2)
    testing.Equal(t, c.Len(), 0)
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
eviction order, capacity 3:
  LRU: d evicts b, b evicts c, e evicts a; keeps [b d e]
  LFU: d evicts b, b evicts c, e evicts d; keeps [a b e]
a typed cache in front of a loader:
  root: admin true
  ann: admin false
  root: admin true
  mallory: load: no user "mallory"
  ann: admin false
  loads: 3 cached: 2 hit rate: 40.0%
  in cache: root true
  in cache: ann false
20000 skewed accesses, capacity 32:
  LRU: 16381 hits, 3619 misses, 3587 evictions, hit rate 81.9%
  LFU: 16609 hits, 3391 misses, 3359 evictions, hit rate 83.0%
bob: not in the cache
```

## Next

- The tour's [generics section](../../guide/tour.md#generics) and `DESIGN.md`
  "Generics" for every constraint.
- [validate](../validate/README.md): generics driven by compile-time
  reflection.
