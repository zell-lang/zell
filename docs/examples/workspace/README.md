# workspace: a library and an application in one repository

Two packages developed together: `geo`, a small library of points,
distances and bounding boxes, and `mapper`, a program that uses it to
describe a trip across Europe. They live in one repository under a
workspace root, `mapper` depends on `geo` by path, and each has its own
tests.

What it shows:

- a workspace root manifest with `[workspace] members`, and what
  `zell run` does there;
- a library package (no `entry`) and an application package (`entry`)
  with a `path` dependency between them;
- namespaces across packages: each package has its own root
  (`geo`, `mapper`), sub-namespaces (`geo.bounds`, `mapper.route`), and
  what `import` and exported names mean between packages;
- `zell test` for each member;
- `zell.lock`, and what changes when `geo` is published on its own.

## Run it

```sh
cd docs/examples/workspace
zell run              # runs mapper, the only member with an entry
zell test geo         # the library's tests
zell test mapper      # the application's tests
```

`zell test` works on one package at a time: run it with the member's
directory, as above, or inside that directory. (At the workspace root
without a path it does not pick the members yet; see
[LANGUAGE_FEEDBACK.md](../../LANGUAGE_FEEDBACK.md) item 47.)

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the workspace root: the list of members |
| `geo/zell.toml` | the library's manifest (root namespace `geo`, no `entry`) |
| `geo/src/geo.zl` | `geo`: `Point`, `Distance` |
| `geo/src/bounds.zl` | `geo.bounds`: `Box`, `Of` |
| `geo/src/geo_test.zl` | the library's tests |
| `mapper/zell.toml` | the application's manifest, with `geo = { path = "../geo" }` |
| `mapper/src/route.zl` | `mapper.route`: stops, legs, the nearest stop |
| `mapper/src/main.zl` | the program |
| `mapper/src/route_test.zl` | the application's tests |

## The workspace root

The root manifest has no `[package]`: it only lists the members. Any
`zell` command run in the root or below finds it by walking up from the
current directory, so `zell build` and `zell run` see both packages as
one graph. The members share one `zell.lock`, written here.

<!-- file: zell.toml -->
```toml title="zell.toml"
# The workspace root: no package of its own, just the list of members.
# zell build, run and test at this level work across all of them, and
# they share one zell.lock here.
[workspace]
members = ["geo", "mapper"]
```

## geo: the library

A package without `entry` is a library: it builds no program, and other
packages use it through `[dependencies]`. Its `namespace` is the root
every one of its files must declare, `geo` or a namespace below it; a
file that declares `namespace mapper` inside `geo/` is a compile error.
`description` and `license` are what an index shows once the package is
published.

<!-- file: geo/zell.toml -->
```toml title="geo/zell.toml"
[package]
name = "geo"
version = "0.2.0"
namespace = "geo"
description = "Points, distances and bounding boxes on a sphere"
license = "MIT"
```

Capitalised names are exported. `Point`, its fields and `Distance` are
visible to dependents; `radians` is private to the namespace `geo`.

<!-- file: geo/src/geo.zl -->
```zell title="geo/src/geo.zl"
// Package geo: points on the Earth and the distance between them. This
// is a library package: its manifest has no `entry`, so it builds no
// program of its own, and other packages use it as a dependency.
namespace geo

// EarthRadiusKm is the mean radius used by Distance.
const EarthRadiusKm = 6371.0

// Point is a position in degrees. Capitalised names are exported: a
// dependent package sees Point, Lat and Lon, but not `radians` below.
struct Point {
    Lat float
    Lon float

    func String() string {
        return fmt.Sprintf("(%.2f, %.2f)", this.Lat, this.Lon)
    }
}

func radians(deg float) float {
    return deg * math.Pi / 180.0
}

// Distance is the great-circle distance in kilometres (the haversine
// formula).
func Distance(a Point, b Point) float {
    var dLat = radians(b.Lat - a.Lat)
    var dLon = radians(b.Lon - a.Lon)
    var s1 = math.Sin(dLat / 2.0)
    var s2 = math.Sin(dLon / 2.0)
    var h = s1 * s1 + math.Cos(radians(a.Lat)) * math.Cos(radians(b.Lat)) * s2 * s2
    return 2.0 * EarthRadiusKm * math.Atan2(math.Sqrt(h), math.Sqrt(1.0 - h))
}
```

`geo.bounds` is a second namespace of the same package. It imports
`geo` like any other namespace, which brings `Point` into scope without
the prefix. `Of` returns `Box!`: an empty list of points has no bounds.

<!-- file: geo/src/bounds.zl -->
```zell title="geo/src/bounds.zl"
// geo.bounds: a sub-namespace of the same package. Inside the package it
// is reached as `geo.bounds`; a dependent imports it the same way.
namespace geo.bounds

import geo

// Box is the smallest latitude/longitude rectangle around some points.
struct Box {
    Min Point
    Max Point

    func Contains(p Point) bool {
        return p.Lat >= this.Min.Lat && p.Lat <= this.Max.Lat && p.Lon >= this.Min.Lon && p.Lon <= this.Max.Lon
    }

    func Center() Point {
        return Point{Lat: (this.Min.Lat + this.Max.Lat) / 2.0, Lon: (this.Min.Lon + this.Max.Lon) / 2.0}
    }
}

// Of returns the box around points; an empty list is an error.
func Of(points []Point) Box! {
    if len(points) == 0 {
        return error("bounds of no points")
    }
    var b = Box{Min: points[0], Max: points[0]}
    for p in points {
        b.Min.Lat = math.Min(b.Min.Lat, p.Lat)
        b.Min.Lon = math.Min(b.Min.Lon, p.Lon)
        b.Max.Lat = math.Max(b.Max.Lat, p.Lat)
        b.Max.Lon = math.Max(b.Max.Lon, p.Lon)
    }
    return b
}
```

The tests are in a namespace of their own, `geo.tests`, so they use the
library the way a dependent would: through its exported names only.
Test files are compiled only when testing their own package, never into
a dependent.

<!-- file: geo/src/geo_test.zl -->
```zell title="geo/src/geo_test.zl"
// The library's own tests: `zell test geo` from the workspace root, or
// `zell test` inside geo/. They are never compiled into dependents.
namespace geo.tests

import geo
import geo.bounds

func near(t *testing.T, got float, want float) {
    testing.True(t, math.Abs(got - want) < 1.0, fmt.Sprintf("got %.1f, want %.1f", got, want))
}

@test func TestDistance(t *testing.T) {
    var paris = Point{Lat: 48.8566, Lon: 2.3522}
    var london = Point{Lat: 51.5074, Lon: -0.1278}
    near(t, Distance(paris, london), 343.6)
    testing.Equal(t, Distance(paris, paris), 0.0)
}

@test func TestBounds(t *testing.T) {
    var b = try Of([Point{Lat: 1, Lon: 5}, Point{Lat: -2, Lon: 7}]) catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, b.Min.Lat, -2.0)
    testing.Equal(t, b.Max.Lon, 7.0)
    testing.True(t, b.Contains(b.Center()), "the center is inside")
    testing.False(t, b.Contains(Point{Lat: 0, Lon: 0}), "outside")
    try Of([]) catch err {
        testing.Contains(t, err.Message(), "no points")
    }
}
```

## mapper: the application

The dependency is a path, relative to the manifest. Isolation between
packages is strict: a file sees the standard library, its own package,
and the packages listed in its own `[dependencies]`, nothing else, not
even a dependency of a dependency. The error for a missing one names the
package to add.

<!-- file: mapper/zell.toml -->
```toml title="mapper/zell.toml"
[package]
name = "mapper"
version = "0.1.0"
namespace = "mapper"
entry = "src/main.zl"

[dependencies]
geo = { path = "../geo" }
```

`mapper.route` imports `geo` and writes `Point` and `Distance` without
the prefix. `var (a, b) = (stops[i - 1], stops[i])` takes a tuple apart.

<!-- file: mapper/src/route.zl -->
```zell title="mapper/src/route.zl"
// mapper.route: routes made of named stops, built on the geo package.
// `import geo` makes geo's exported names usable without the prefix;
// the full form (geo.Distance) works too, but only for packages listed
// in this package's [dependencies].
namespace mapper.route

import geo

struct Stop {
    Name string
    At   Point
}

struct Leg {
    From     string
    To       string
    Km       float
}

func Legs(stops []Stop) []Leg {
    var out []Leg = []
    for i in 1..len(stops) {
        var (a, b) = (stops[i - 1], stops[i])
        out = append(out, Leg{From: a.Name, To: b.Name, Km: Distance(a.At, b.At)})
    }
    return out
}

func Length(stops []Stop) float {
    var km = 0.0
    for l in Legs(stops) {
        km += l.Km
    }
    return km
}

// Nearest returns the stop closest to p.
func Nearest(stops []Stop, p Point) Stop! {
    if len(stops) == 0 {
        return error("no stops")
    }
    var best = stops[0]
    for s in stops {
        if Distance(s.At, p) < Distance(best.At, p) {
            best = s
        }
    }
    return best
}

func Points(stops []Stop) []Point {
    var out []Point = []
    for s in stops {
        out = append(out, s.At)
    }
    return out
}
```

`main` returns `void!`: if it fails, the program prints the error and
exits with status 1. It imports namespaces from both packages; to the
code, a namespace from another package looks the same as one of its own.

<!-- file: mapper/src/main.zl -->
```zell title="mapper/src/main.zl"
// mapper: the application package of the workspace. It depends on geo
// by path (see mapper/zell.toml) and is what `zell run` at the
// workspace root runs, because it is the only member with an entry.
namespace mapper

import io
import geo
import geo.bounds
import mapper.route

func main() void! {
    var trip = [
        Stop{Name: "Lisbon", At: Point{Lat: 38.7223, Lon: -9.1393}},
        Stop{Name: "Madrid", At: Point{Lat: 40.4168, Lon: -3.7038}},
        Stop{Name: "Paris", At: Point{Lat: 48.8566, Lon: 2.3522}},
        Stop{Name: "Berlin", At: Point{Lat: 52.52, Lon: 13.405}},
        Stop{Name: "Vienna", At: Point{Lat: 48.2082, Lon: 16.3738}},
    ]
    println("route:")
    for l in Legs(trip) {
        println(fmt.Sprintf("  %-7s -> %-7s %7.1f km", l.From, l.To, l.Km))
    }
    println(fmt.Sprintf("  total %.1f km", Length(trip)))

    var box = Of(Points(trip))?("bounding box")
    println("bounding box:", box.Min.String(), "to", box.Max.String())
    var mid = box.Center()
    var near = Nearest(trip, mid)?
    println("center", mid.String(), "is nearest to", near.Name)

    var zurich = Point{Lat: 47.3769, Lon: 8.5417}
    println("Zurich inside the box:", box.Contains(zurich))
}
```

<!-- file: mapper/src/route_test.zl -->
```zell title="mapper/src/route_test.zl"
// The application's tests, against geo as a dependency: `zell test
// mapper` from the workspace root.
namespace mapper.tests

import geo
import mapper.route

func stop(name string, lat float, lon float) Stop {
    return Stop{Name: name, At: Point{Lat: lat, Lon: lon}}
}

@test func TestLegsAndLength(t *testing.T) {
    var stops = [stop("a", 0, 0), stop("b", 0, 1), stop("c", 0, 2)]
    var legs = Legs(stops)
    testing.Equal(t, len(legs), 2)
    testing.Equal(t, legs[1].From, "b")
    // One degree of longitude on the equator is about 111.2 km.
    testing.True(t, math.Abs(Length(stops) - 222.4) < 0.5, "two degrees along the equator")
}

@test func TestNearest(t *testing.T) {
    var stops = [stop("north", 10, 0), stop("south", -10, 0)]
    var s = try Nearest(stops, Point{Lat: -3, Lon: 1}) catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, s.Name, "south")
    try Nearest([], Point{}) catch err {
        testing.Contains(t, err.Message(), "no stops")
    }
}
```

## zell.lock

The first build writes `zell.lock` in the workspace root: every package
of the graph with its version, source and checksum. Commit it. For this
workspace it looks like this (checksums shortened):

```yaml
# zell.lock: generated by `zell`, do not edit by hand.
packages:
- checksum: sha256:b8735c17...
  name: geo
  source: path:geo
  version: 0.2.0
- checksum: sha256:d7f89f0e...
  dependencies:
  - geo
  name: mapper
  source: path:mapper
  version: 0.1.0
version: 1
```

(The copy of this example in the repository has no lock file: the test
that runs it builds a fresh copy, and the checksums change with every
edit of the sources.)

## Publishing geo

There is no central registry: a package is identified by its repository
path, and a version is a git tag. To publish `geo` on its own, put it in
its own repository (`github.com/you/geo`), check that its tests pass,
and push a tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

A published package cannot have `path` dependencies, so `mapper` then
switches from the path to the repository and a version constraint, and
`zell install` records the exact version and checksum in `zell.lock`:

```toml
[dependencies]
"github.com/you/geo" = "^0.2"
```

`import geo` stays as it is: code imports a package by its root
namespace, not by its path. See [Packages](../../guide/packages.md) and
[PACKAGES.md](../../PACKAGES.md) for version constraints, the module
proxy and vendoring.

## Output

<!-- file: expected.out -->
```text title="expected.out"
route:
  Lisbon  -> Madrid    502.4 km
  Madrid  -> Paris    1052.9 km
  Paris   -> Berlin    877.5 km
  Berlin  -> Vienna    523.5 km
  total 2956.3 km
bounding box: (38.72, -9.14) to (52.52, 16.37)
center (45.62, 3.62) is nearest to Paris
Zurich inside the box: true
```

## Next

- [Packages and projects](../../guide/packages.md): manifests,
  dependencies, workspaces and the commands.
- [Testing](../../guide/testing.md): `zell test` in detail.
