# Web services with `web`

`web` is the batteries-included HTTP framework of the standard
library, in the spirit of ASP.NET Core minimal APIs: routes map to plain
functions or controller methods, parameters are bound from the path,
query and JSON body by their types (checked at compile time), results
are written with content negotiation, errors become RFC 9457 Problem
Details, and dependency injection, validation, authentication,
middleware and an OpenAPI document come with it. Underneath is
`net.http` (HTTP/1.1 and HTTP/2, TLS), which you can also use
directly. The design is [docs/FRAMEWORK.md](../FRAMEWORK.md).

## Hello, web

```zell
// check-only
namespace guide.web.hello

import web

async func main() void! {
    var app = NewApp()
    app.MapGet("/", () => "Hello, web!")
    app.MapGet("/hello/{name}", (name string) => "Hello, " + name + "!")
    await app.Run(":8080")?
}
```

`zell run` it and open `http://localhost:8080/hello/ann`. Strings are
written as `text/plain`; any other value as JSON.

Everything on this page can be tested without a socket: `TestServer(app)`
builds the app and returns a client that calls it in memory. The rest of
the examples use it, so they run as they are:

```zell
namespace guide.web.minimal

import io
import web

struct Todo {
    Id    int
    Title string
    Done  bool
}

async func main() void! {
    var app = NewApp()
    app.MapGet("/todos/{id}", (id int) => Todo{Id: id, Title: "write docs"})
    app.MapGet("/sum", (a int, b int) => a + b)   // query parameters: /sum?a=1&b=2
    var c = TestServer(app)?
    var r = await c.Get("/todos/3")
    println(r.Status, r.Header("Content-Type"), r.Body)
    r = await c.Get("/sum?a=20&b=22")
    println(r.Status, r.Body)
    r = await c.Get("/todos/x")
    println(r.Status)
}
// Output:
// 200 application/json {"Id":3,"Title":"write docs","Done":false}
// 200 42
// 400
```

A parameter that does not convert (`/todos/x` for an `int`) is a 400
Problem Details response, not a crash.

## Controllers

For more than a handful of routes, group them in a controller: a struct
with a `@route` prefix whose methods carry `@get`, `@post`, `@put`,
`@patch` or `@delete`. A `@body` parameter is read from the JSON body;
a `Result<T>` return value chooses the status.

```zell
namespace guide.web.controllers

import io
import web

struct Book {
    Id    int
    Title string
}

@route("/api/books")
struct BooksController {
    @get("{id}")
    func Get(id int) Result<Book> {
        if id != 1 {
            return NotFound()
        }
        return Ok(Book{Id: 1, Title: "Dune"})
    }

    @post("")
    func Create(@body b Book) Result<Book> {
        return Created(b)
    }
}

async func main() void! {
    var app = NewApp()
    app.MapController<BooksController>()
    var c = TestServer(app)?
    var r = await c.Get("/api/books/1")
    println(r.Status, r.Body)
    r = await c.Get("/api/books/2")
    println(r.Status, r.Header("Content-Type"))
    r = await c.Post("/api/books", "{\"Id\": 2, \"Title\": \"Emma\"}")
    println(r.Status, r.Body)
}
// Output:
// 200 {"Id":1,"Title":"Dune"}
// 404 application/problem+json
// 201 {"Id":2,"Title":"Emma"}
```

Handlers may be `async`, may return `T!` (an error becomes a 500 Problem whose `detail` is a generic "internal error" outside development,
or the status of a `web.Error(404, "...")`), and may take a `*Context`
for the raw request and response.

## Validation

`@validate` on fields (and parameters) declares the rules; a request
that breaks them gets one 400 response listing every failed field.

```zell
namespace guide.web.validation

import io
import web

struct SignUp {
    @validate(required: true, email: true) Email string
    @validate(minLength: 8) Password string
    @validate(range: [13, 130]) Age int
}

@route("/users")
struct UsersController {
    @post("")
    func Create(@body s SignUp) string {
        return "welcome " + s.Email
    }
}

async func main() void! {
    var app = NewApp()
    app.MapController<UsersController>()
    var c = TestServer(app)?
    var r = await c.Post("/users", "{\"Email\": \"ann@example.com\", \"Password\": \"correct horse\", \"Age\": 30}")
    println(r.Status, r.Body)
    r = await c.Post("/users", "{\"Email\": \"nope\", \"Password\": \"short\", \"Age\": 7}")
    println(r.Status, r.Header("Content-Type"))
}
// Output:
// 200 welcome ann@example.com
// 400 application/problem+json
```

## Services and dependency injection

Register services on `app.Services` as singletons (one per app), scoped
(one per request, dropped with it) or transient (new on every
resolution). Controllers receive them in their fields; minimal API
handlers call `Resolve<T>()`.

```zell
namespace guide.web.di

import io
import web

interface Clock {
    func Now() string
}

struct FixedClock {
    func Now() string {
        return "12:00"
    }
}

struct Visits {
    n int
}

@route("/")
struct HomeController {
    clock  Clock
    visits *Visits

    @get("")
    func Index() string {
        this.visits.n += 1
        return fmt.Format("visit {0} at {1}", this.visits.n, this.clock.Now())
    }
}

async func main() void! {
    var app = NewApp()
    app.Services.AddSingleton<Clock, FixedClock>()
    app.Services.AddSingleton<Visits>()
    app.MapController<HomeController>()
    var c = TestServer(app)?
    println((await c.Get("/")).Body)
    println((await c.Get("/")).Body)
}
// Output:
// visit 1 at 12:00
// visit 2 at 12:00
```

## Middleware and the rest

`app.Use(func(c *Context, next *Next) Task<void!>)` wraps every request;
ready-made ones are `UseLogging`, `UseRecover`, `UseCors`, `UseGzip`,
`UseStaticFiles`, `UseRateLimit`, `UseRequestId`, `UseTracing`,
`UseAuth` (JWT bearer tokens, with `RequireAuthorization` on endpoints),
`UseBodyLimit` and `UseTimeout`. `MapOpenApi("/openapi.json")` serves an
OpenAPI 3.1 document generated from the routes and types, and
`MapSwagger` a UI for it; `AddHealthCheck` and `MapHealthChecks` serve
readiness checks. Configuration comes from `appsettings.json`,
`appsettings.<Environment>.json` and environment variables
(`web.config`).

```zell
// check-only
namespace guide.web.production

import web

async func main() void! {
    var app = NewApp()
    app.UseRecover().UseLogging().UseRequestId(RequestIdOptions{}).UseGzip()
    app.MapGet("/healthz", () => "ok")
    app.MapOpenApi("/openapi.json")
    var port = os.Getenv("PORT")
    if port == "" {
        port = "8080"
    }
    await app.Run(":" + port)?
}
```

## Plain `net.http`

Without the framework, a `http.Router` maps patterns to handler
functions and `http.Server` serves it; `http.Get`, `http.Post` and
`http.Client` are the client side:

```zell
namespace guide.web.nethttp

import io
import net

async func main() void! {
    var r = http.NewRouter()
    r.Get("/ping", async func(req *http.Request, res *http.Response) void! {
        res.Text(200, "pong")
    })
    var srv = http.Server{Addr: "127.0.0.1:0", Handler: r}
    srv.Listen()?
    var serving = srv.Serve()
    var res = await http.Get(fmt.Format("http://127.0.0.1:{0}/ping", srv.Port()))?
    println(res.Status, res.Body)
    await srv.Shutdown()
    await serving?
}
// Output:
// 200 pong
```
