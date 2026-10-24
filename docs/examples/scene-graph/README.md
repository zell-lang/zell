# scene-graph: memory without a garbage collector

A scene graph, the tree of objects a game or an editor draws, is the
classic place where memory management gets interesting: parents own
children, children want to know their parent, objects refer to each
other sideways, and thousands of short-lived objects appear every frame.
This example builds one and prints a line from `Drop` at the exact
moment each node is freed, so the output is a record of when Zell frees
memory, and why.

What it shows:

- values (`Vec2`, `Transform`) are copied; references (`*Node`) are
  shared and reference counted;
- reassigning the last reference frees the old object on that line;
- `Drop` runs exactly once, in a deterministic order;
- `weak *Node` for the parent link: it does not keep the parent alive
  and reads as `nil` once the parent is gone;
- an intentional cycle (`Target`) that reference counting cannot free,
  `@allowCycle`, and the bounded cycle collector freeing it;
- what `zell run --leak-check` reports when a cycle is left behind;
- an arena reused for each frame's particles: one allocation, one free.

## Run it

```sh
cd docs/examples/scene-graph
zell run
zell run --leak-check
zell test
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `scene`) |
| `src/node.zl` | `scene.node`: `Vec2`, `Transform`, `Node`, tree operations |
| `src/particles.zl` | `scene.particles`: particles in an arena |
| `src/main.zl` | the four walkthroughs |
| `src/node_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "scene-graph"
version = "0.1.0"
namespace = "scene"
entry = "src/main.zl"
```

## src/node.zl: values, references, weak parents

`Vec2` and `Transform` are plain structs used by value: they live inside
whatever holds them (here, inside a `Node`), assigning one copies it,
and they have no reference count. `Node` is used by reference: created
with `new`, shared by every copy of the pointer, freed when the last
reference disappears.

The tree has one owner per node: `Children []*Node` are strong
references, from parent to child. The way back up, `Parent`, is
`weak *Node`. If it were strong, every parent and child would keep each
other alive and no tree would ever be freed. A weak reference converts
from `*Node` implicitly, and reading it gives a `*Node` that is `nil`
once the object is gone; `Path` and `WorldPos` walk up that way.

`Target` is the opposite case: a "look at" link that can point anywhere,
including back at the node that points to it. The compiler sees that
`Node` can form a cycle through it and warns (`W0001 type Node can form
a reference cycle through field Target`); `@allowCycle` records that the
cycles are intended. Objects of such a type are the only ones the cycle
collector ever examines.

<!-- file: src/node.zl -->
```zell title="src/node.zl"
// The scene graph: a tree of nodes. Parents own their children (strong
// references down the tree); children point back at their parent with a
// weak reference, which does not keep the parent alive.
namespace scene.node

import io

// Vec2 and Transform are values: assigning one copies it, like a Go
// struct, and they have no reference count at all.
struct Vec2 {
    X int
    Y int

    func Add(o Vec2) Vec2 {
        return Vec2{X: this.X + o.X, Y: this.Y + o.Y}
    }
}

struct Transform {
    Pos   Vec2
    Scale int
}

// Node is a reference type: it is always created with `new`, and every
// copy of a *Node is the same object, freed when the last copy is gone.
//
// Target ("look at this node") is a strong reference that can point
// anywhere in the scene, so two nodes can look at each other: a cycle
// that reference counting alone never frees. The compiler warns about
// such a type (W0001); @allowCycle says the cycles are intended, and the
// cycle collector frees the ones that become unreachable.
@allowCycle
struct Node {
    Name     string
    Local    Transform
    Parent   weak *Node
    Children []*Node
    Target   *Node

    // Drop runs exactly once, when the node is freed. Printing here makes
    // the moment visible; a real node would release a GPU buffer.
    func Drop() {
        println("  drop", this.Name)
    }
}

func New(name string, x int, y int) *Node {
    return new Node{Name: name, Local: Transform{Pos: Vec2{X: x, Y: y}, Scale: 1}}
}

// Attach makes child the last child of parent. A *Node converts to a
// weak *Node implicitly.
func Attach(parent *Node, child *Node) {
    child.Parent = parent
    parent.Children = append(parent.Children, child)
}

// Detach takes n out of its parent's children. The caller's reference
// is then the only thing keeping n (and its subtree) alive.
func Detach(n *Node) {
    var p = n.Parent
    if p == nil {
        return
    }
    var kept []*Node = []
    for c in p.Children {
        if c != n {
            kept = append(kept, c)
        }
    }
    p.Children = kept
    n.Parent = nil
}

// Path walks up the weak parent links. Reading a weak reference gives a
// *Node, or nil once the parent is gone (or was never set).
func Path(n *Node) string {
    var parts = [n.Name]
    var p = n.Parent
    for p != nil {
        parts = append(parts, p.Name)
        p = p.Parent
    }
    slices.Reverse(parts)
    return strings.Join(parts, "/")
}

// WorldPos adds up the local positions from n to the root.
func WorldPos(n *Node) Vec2 {
    var pos = n.Local.Pos
    var p = n.Parent
    for p != nil {
        pos = pos.Add(p.Local.Pos)
        p = p.Parent
    }
    return pos
}

func Print(n *Node, depth int) {
    var w = WorldPos(n)
    println(fmt.Format("  {0}{1} at ({2}, {3})", strings.Repeat("  ", depth), n.Name, w.X, w.Y))
    for c in n.Children {
        Print(c, depth + 1)
    }
}
```

## src/particles.zl: an arena per frame

Particles live for one frame. Allocating and reference counting each of
them would be wasted work, so the frame's particles are one
`a.Alloc<Particle>(n)` slice inside an arena, and `a.Free()` releases
them all at once; the arena is then reused for the next frame. `&ps[i]`
is an interior pointer (into the middle of the slice), which is fine in
a local variable while `ps` is alive.

<!-- file: src/particles.zl -->
```zell title="src/particles.zl"
// Arenas: many short-lived objects with one lifetime (a frame) are
// allocated in one region and freed all at once, with no reference
// counting on the way.
namespace scene.particles

import io

struct Particle {
    Id int
    X  int
    Y  int
    VX int
    VY int
}

// spawn allocates n particles in the arena. a.Alloc<T>(n) returns a []T
// whose buffer lives inside the arena: one pointer bump for the whole
// frame, no count, no individual free. (a.New<T>() gives a single *T.)
func spawn(a arena.Arena, frame int, n int) []Particle {
    var ps = a.Alloc<Particle>(n)
    for i in 0..n {
        ps[i].Id = i
        ps[i].VX = (i % 7) - 3
        ps[i].VY = frame + 1
    }
    return ps
}

// Frames simulates a few frames. Each frame allocates its particles in
// the arena and frees them together at the end of the frame; the arena
// is then reused. Nothing allocated in an arena may be used after Free.
func Frames(frames int, n int) {
    println("arenas, one region reused per frame:")
    var a = arena.New()
    for f in 0..frames {
        var ps = spawn(a, f, n)
        var sumX = 0
        for i in 0..len(ps) {
            var p = &ps[i]     // an interior pointer, used while ps is alive
            p.X += p.VX * 10 + f
            p.Y += p.VY * 10
            sumX += p.X
        }
        println(fmt.Format("  frame {0}: {1} particles, sum x {2}, arena objects {3}", f, len(ps), sumX, a.Len()))
        a.Free()           // every particle of this frame, at once
    }
}
```

## src/main.zl: four walkthroughs

**Values and references.** Changing the copy `b` leaves `a` alone;
changing `same` changes `n`, because they are one node. `lamp` has two
references, so `n = New("bulb", ...)` does not free it; `same = n`
removes the last one and `drop lamp` is printed on that line. `bulb` is
freed when its two variables go out of scope at the end of `values`.

**A tree.** `Detach(house)` takes the subtree out of the scene, and the
local variable `house` keeps it alive on its own. Setting `house = nil`
frees `house` and then its child `door`: a node's fields, including its
children, are released after its `Drop` runs. Releasing `world` and then
`root` frees the scene from the top, except `tree`, which a local
variable still holds; its weak `Parent` now reads `nil`.

**A cycle.** `camera` and `player` target each other, so after both
variables are cleared each still has a count of one and nothing is
dropped. `runtime.CollectCycles()` finds the two objects that only
reference each other, runs both `Drop`s and frees them. By default
(`ZELL_CYCLES=auto`) the collector also runs by itself once enough
candidates pile up, and once more at exit under `--leak-check`; calling
it here makes the moment visible.

**Arenas.** Three frames of 1000 particles in one reused arena.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// scene-graph: memory without a garbage collector, step by step. Every
// "drop" line in the output is printed by Node.Drop at the exact moment
// the node is freed.
namespace scene

import io
import scene.node
import scene.particles

func values() {
    println("values and references:")
    var a = Transform{Pos: Vec2{X: 1, Y: 1}, Scale: 1}
    var b = a              // a copy: changing b leaves a alone
    b.Pos.X = 50
    println("  transforms:", a.Pos.X, b.Pos.X)

    var n = New("lamp", 0, 0)
    var same = n           // the same node: one object, two references
    same.Local.Pos.X = 50
    println("  nodes:", n.Local.Pos.X, same.Local.Pos.X)
    // Reassignment releases the old object. lamp has two references:
    // the first assignment leaves one, the second frees it right there.
    n = New("bulb", 0, 0)
    same = n
    println("  lamp is gone, n is", n.Name)
}                          // bulb: its last references end with the block

func tree() {
    println("a tree with weak parents:")
    var root = New("scene", 0, 0)
    var world = New("world", 10, 0)
    var house = New("house", 5, 5)
    var tree = New("tree", -3, 0)
    Attach(root, world)
    Attach(world, house)
    Attach(house, New("door", 1, 0))
    Attach(world, tree)
    Print(root, 0)

    // Detach a subtree but keep a reference: it stays alive, alone.
    Detach(house)
    println("  detached:", Path(house), "- world has", len(world.Children), "child")
    house = nil            // the last reference: house and door go now
    println("  house released")

    // Drop the whole scene while still holding a leaf. Only the leaf
    // survives, and its weak parent link now reads nil.
    world = nil
    root = nil
    println("  scene released; tree's parent is nil:", tree.Parent == nil, "path", Path(tree))
}

func cycle() {
    println("a cycle, and the cycle collector:")
    var camera = New("camera", 0, 10)
    var player = New("player", 0, 0)
    camera.Target = player     // the camera looks at the player...
    player.Target = camera     // ...and the player at the camera
    camera = nil
    player = nil
    // Both counts are still 1 (each holds the other): nothing dropped.
    println("  both released, nothing dropped yet")
    // The collector looks only at objects of cyclic types whose count
    // went down without reaching zero, finds that these two only
    // reference each other, runs their Drop and frees them.
    println("  collected:", runtime.CollectCycles())
}

func main() {
    values()
    tree()
    cycle()
    Frames(3, 1000)
    println("done")
}
```

## The leak check

`zell run --leak-check` lists every object still alive when the program
ends and exits with status 66 if there are any. This program leaves
nothing behind. Remove the `runtime.CollectCycles()` line and run it
with the collector switched to manual mode, and the cycle shows up,
with the field that closes it:

```sh
ZELL_CYCLES=manual zell run --leak-check
```

```text
leak: 2 objects
  2 x scene.node.Node
  cycle through Node.Target (make one field of the cycle `weak *T` to break it)
  2 of them in unreachable reference cycles the cycle collector can free (ZELL_CYCLES=auto or runtime.CollectCycles())
```

## src/node_test.zl: tests

The tests check the tree operations and the two memory guarantees: a
weak parent reads `nil` as soon as the parent is freed, and a cycle is
collected exactly once.

<!-- file: src/node_test.zl -->
```zell title="src/node_test.zl"
// Tests for the tree operations and for what weak references and the
// cycle collector guarantee. Node.Drop prints, which `zell test -v` shows.
namespace scene.tests

import scene.node

@test func TestPathAndWorldPos(t *testing.T) {
    var root = New("root", 1, 1)
    var arm = New("arm", 2, 0)
    var hand = New("hand", 0, 3)
    Attach(root, arm)
    Attach(arm, hand)
    testing.Equal(t, Path(hand), "root/arm/hand")
    var w = WorldPos(hand)
    testing.Equal(t, w.X, 3)
    testing.Equal(t, w.Y, 4)
}

@test func TestDetach(t *testing.T) {
    var root = New("root", 0, 0)
    var a = New("a", 0, 0)
    Attach(root, a)
    Attach(root, New("b", 0, 0))
    Detach(a)
    testing.Equal(t, len(root.Children), 1)
    testing.Equal(t, root.Children[0].Name, "b")
    testing.True(t, a.Parent == nil, "a detached node has no parent")
    testing.Equal(t, Path(a), "a")
}

@test func TestWeakParentReadsNilOnceFreed(t *testing.T) {
    var root = New("root", 0, 0)
    var leaf = New("leaf", 0, 0)
    Attach(root, leaf)
    testing.True(t, leaf.Parent != nil, "the parent is alive")
    root = nil
    testing.True(t, leaf.Parent == nil, "a weak reference does not keep the parent alive")
}

func makeCycle() {
    var a = New("a", 0, 0)
    var b = New("b", 0, 0)
    a.Target = b
    b.Target = a
}

@test func TestCycleIsCollected(t *testing.T) {
    makeCycle()
    testing.Equal(t, runtime.CollectCycles(), 2)
    testing.Equal(t, runtime.CollectCycles(), 0)
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
values and references:
  transforms: 1 50
  nodes: 50 50
  drop lamp
  lamp is gone, n is bulb
  drop bulb
a tree with weak parents:
  scene at (0, 0)
    world at (10, 0)
      house at (15, 5)
        door at (16, 5)
      tree at (7, 0)
  detached: house - world has 1 child
  drop house
  drop door
  house released
  drop scene
  drop world
  scene released; tree's parent is nil: true path tree
  drop tree
a cycle, and the cycle collector:
  both released, nothing dropped yet
  drop camera
  drop player
  collected: 2
arenas, one region reused per frame:
  frame 0: 1000 particles, sum x -30, arena objects 1
  frame 1: 1000 particles, sum x 970, arena objects 1
  frame 2: 1000 particles, sum x 1970, arena objects 1
done
```

## Next

- [Memory without a garbage collector](../../guide/memory.md): `Drop`,
  `weak`, the cycle collector, arenas and interior pointers in detail.
- [inventory-db](../inventory-db/README.md): `Drop` closing real
  resources (database rows and connections).
