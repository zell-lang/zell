# bookshelf-api: a JSON API over HTTP

A small REST service for a list of books: create, list, read, update and
delete over HTTP with JSON bodies. It is written on the standard
`net.http` package (router, middleware, server and client), with the
handlers depending on a repository interface rather than on storage.

The program is its own client: `main` starts the server on a free
loopback port, sends a series of requests the way an integration test
would, shuts the server down gracefully while one request is still in
progress, and prints the access log. So `zell run` really serves HTTP, and
its output is the same on every run.

What it shows:

- `http.NewRouter` with path parameters (`/books/{id}`), query values,
  `req.JSON<T>()` and `res.SendJSON<T>(...)` generated from `@json`
  attributes;
- middleware as `func(next Handler) Handler`: a request id, an access
  log, and the one place where errors become HTTP statuses;
- a repository interface whose methods return `Task<T>`, implemented by
  an in-memory store guarded by a `task.Mutex`;
- input validation with a typed error that names the field;
- request-scoped values (`req.Set` / `req.Value`) passing data between
  middleware;
- `http.Server` on port 0, `Shutdown()` that waits for requests in flight;
- handler tests without a network, through `net.http.test`.

## Run it

```sh
cd docs/examples/bookshelf-api
zell run
zell test
```

A deployed service would listen on a fixed address and wait for a signal
before shutting down; both places are marked in `main`. With the server
running, any HTTP client works:

```sh
curl -s -X POST localhost:8080/books -d '{"title":"Dune","author":"Frank Herbert","year":1965}'
curl -s localhost:8080/books?author=herbert
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `bookshelf`) |
| `src/main.zl` | wiring, the in-process client, graceful shutdown |
| `src/handlers.zl` | `bookshelf.api`: routes and handlers |
| `src/middleware.zl` | `bookshelf.api`: request id, logging, error mapping |
| `src/model.zl` | `bookshelf.model`: `Book`, input validation, the repository |
| `src/api_test.zl` | handler tests (`zell test`) |

`handlers.zl` and `middleware.zl` are two files of one namespace,
`bookshelf.api`: a namespace may span files, and the files see each
other's private names.

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "bookshelf-api"
version = "0.1.0"
namespace = "bookshelf"
entry = "src/main.zl"
```

## src/model.zl: books and the repository

`Book` is what the API returns; `BookInput` is what it accepts. They are
separate types because a client must not choose the id, and with a
separate type it cannot.

`BookInput.Validate` returns a `ValidationError` that names the field.
The handlers do not turn it into a status code; the error middleware
does, by its type.

`Repository` is the interface the handlers depend on. Its methods are
`async` (`async func Get(id int) Book!` in an interface means `func
Get(id int) Task<Book!>`), so an implementation backed by a database (see
[inventory-db](../inventory-db/README.md)) can do I/O without changing a
handler. `MemoryRepo` implements them with `async` methods.

The server runs handlers on several worker threads at once, so the map is
guarded by a `task.Mutex`. Unlike an OS mutex, a task waiting for it is
suspended and its thread runs other tasks meanwhile. `defer` releases it
on every path, including the early error returns.

<!-- file: src/model.zl -->
```zell title="src/model.zl"
// The domain: books, the input the API accepts, the errors handlers can
// return, and the repository interface with an in-memory implementation.
namespace bookshelf.model

// Book is what the API returns. The @json names are the wire format;
// the Go-style field names stay idiomatic in code.
struct Book {
    @json(name: "id")                    Id     int
    @json(name: "title")                 Title  string
    @json(name: "author")                Author string
    @json(name: "year")                  Year   int
    @json(name: "tags", omitEmpty: true) Tags   []string
}

// BookInput is the body of POST and PUT. It has no id: clients do not
// choose ids, and a separate type makes that impossible to get wrong.
struct BookInput {
    @json(name: "title")  Title  string
    @json(name: "author") Author string
    @json(name: "year")   Year   int
    @json(name: "tags")   Tags   []string

    // Validate returns the first problem. The handlers do not check
    // fields themselves: the error type tells the middleware to answer
    // 400 with the field name.
    func Validate() void! {
        if strings.Trim(this.Title) == "" {
            return ValidationError{Field: "title", Reason: "is required"}
        }
        if strings.Trim(this.Author) == "" {
            return ValidationError{Field: "author", Reason: "is required"}
        }
        if this.Year < 1450 || this.Year > 2100 {
            return ValidationError{
                Field: "year",
                Reason: fmt.Format("{0} is not a plausible year", this.Year),
            }
        }
    }
}

const ErrBookNotFound = error("not found")

struct ValidationError {
    Field  string
    Reason string

    func Message() string {
        return this.Field + " " + this.Reason
    }
}

// Repository is what the handlers depend on. Its methods are async (each
// one returns a task), so a database-backed implementation (see the
// inventory-db example) can drop in without changing a handler.
interface Repository {
    async func List() []Book
    async func Get(id int) Book!
    async func Create(in BookInput) Book
    async func Update(id int, in BookInput) Book!
    async func Delete(id int) void!
}

// MemoryRepo keeps books in a map. Requests are served by several worker
// threads at once, so every access holds the mutex; task.Mutex suspends
// the waiting task instead of blocking its thread.
struct MemoryRepo {
    mu     task.Mutex
    books  map[int]Book
    nextId int

    static func New() *MemoryRepo {
        return new MemoryRepo{books: map[int]Book{}, nextId: 1}
    }

    async func List() []Book {
        await this.mu.Lock()
        defer this.mu.Unlock()
        // Map order is unspecified; sorting by key gives clients a stable
        // order (by id, which is also creation order).
        return maps.SortedValues(this.books)
    }

    async func Get(id int) Book! {
        await this.mu.Lock()
        defer this.mu.Unlock()
        var (b, ok) = this.books[id]
        if !ok {
            return errors.Wrap(ErrBookNotFound, fmt.Format("book {0}", id))
        }
        return b
    }

    async func Create(in BookInput) Book {
        await this.mu.Lock()
        defer this.mu.Unlock()
        var b = Book{
            Id: this.nextId,
            Title: in.Title,
            Author: in.Author,
            Year: in.Year,
            Tags: in.Tags,
        }
        this.books[b.Id] = b
        this.nextId += 1
        return b
    }

    async func Update(id int, in BookInput) Book! {
        await this.mu.Lock()
        defer this.mu.Unlock()
        var (_, ok) = this.books[id]
        if !ok {
            return errors.Wrap(ErrBookNotFound, fmt.Format("book {0}", id))
        }
        var b = Book{Id: id, Title: in.Title, Author: in.Author, Year: in.Year, Tags: in.Tags}
        this.books[id] = b
        return b
    }

    async func Delete(id int) void! {
        await this.mu.Lock()
        defer this.mu.Unlock()
        var (_, ok) = this.books[id]
        if !ok {
            return errors.Wrap(ErrBookNotFound, fmt.Format("book {0}", id))
        }
        delete(this.books, id)
    }
}
```

## src/handlers.zl: routes and handlers

A handler is an `async func(req *http.Request, res *http.Response)
void!`. Each one here is three steps: decode (`idParam`, `decode`), call
the repository, encode (`res.SendJSON`). Every failure is returned with
`?`; no handler writes an error response itself.

`req.JSON<BookInput>()` and `res.SendJSON(200, b)` are generated at
compile time from the `@json` attributes. A body that is not JSON becomes
a `BadRequest`, a well-formed body with bad values a `ValidationError`.

The handlers are closures over `repo`: for a service of this size, that
is all the dependency injection needed.

<!-- file: src/handlers.zl -->
```zell title="src/handlers.zl"
// The HTTP layer: routes and handlers. A handler decodes the request,
// calls the repository and encodes the answer; it never decides status
// codes for failures. It returns the error, and the middleware maps it.
namespace bookshelf.api

import net

import bookshelf.model

// ErrorBody is the JSON shape of every error answer.
struct ErrorBody {
    @json(name: "error")                  Error string
    @json(name: "field", omitEmpty: true) Field string
}

// BadRequest is a malformed request (not JSON, a non-numeric id). It is
// separate from ValidationError, which is about well-formed but invalid
// values.
struct BadRequest {
    Text string

    func Message() string {
        return this.Text
    }
}

func idParam(req *http.Request) int! {
    var raw = req.Param("id")
    return try strconv.Atoi(raw) catch err {
        return BadRequest{Text: "id must be a number, got " + strconv.Quote(raw)}
    }
}

// decode parses and validates the body. req.JSON<T>() is generated for
// BookInput at compile time from its @json attributes.
func decode(req *http.Request) BookInput! {
    var in = try req.JSON<BookInput>() catch err {
        return BadRequest{Text: "invalid JSON: " + err.Message()}
    }
    in.Validate()?
    return in
}

// Routes builds the router. The handlers are closures over repo: that is
// all the dependency injection a service of this size needs.
func Routes(repo Repository) *http.Router {
    var r = http.NewRouter()
    r.Get(
        "/books",
        async func(req *http.Request, res *http.Response) void! {
            var books = await repo.List()
            // ?author= filters on the server; an absent parameter is "".
            var author = req.QueryValue("author")
            if author != "" {
                books = slices.Filter(
                    books,
                    (b Book) => strings.Contains(
                        strings.ToLower(b.Author),
                        strings.ToLower(author),
                    ),
                )
            }
            res.SendJSON(200, books)
        },
    )
    r.Get("/books/{id}", async func(req *http.Request, res *http.Response) void! {
        var b = await repo.Get(idParam(req)?)?
        res.SendJSON(200, b)
    })
    r.Post("/books", async func(req *http.Request, res *http.Response) void! {
        var b = await repo.Create(decode(req)?)
        res.SetHeader("Location", fmt.Format("/books/{0}", b.Id))
        res.SendJSON(201, b)
    })
    r.Put("/books/{id}", async func(req *http.Request, res *http.Response) void! {
        var id = idParam(req)?
        var b = await repo.Update(id, decode(req)?)?
        res.SendJSON(200, b)
    })
    r.Delete("/books/{id}", async func(req *http.Request, res *http.Response) void! {
        await repo.Delete(idParam(req)?)?
        res.Status = 204
    })
    r.Get("/health", async func(req *http.Request, res *http.Response) void! {
        res.Text(200, "ok\n")
    })
    return r
}
```

## src/middleware.zl: around every handler

A middleware is a function from the next handler to a new handler;
`Router.Use` applies them in order, the first one outermost. `main`
installs three:

1. `RequestID` numbers each request and returns the number in the
   `X-Request-Id` header (a `sync.AtomicInt`, since requests run in
   parallel).
2. `Logging` records method, path, final status and size after the
   request is done.
3. `Errors` catches what handlers return and answers with JSON:
   `ValidationError` and `BadRequest` become 400, anything wrapping
   `ErrBookNotFound` becomes 404, and everything else 500 with a generic
   message. `errors.As` and `errors.Is` search the whole chain of
   wrappers, so the mapping keeps working when handlers add context with
   `?("...")`.

The internal cause of a 500 must reach the log but not the client.
`Errors` stores it in the request with `req.Set`, and `Logging`, which
runs after it on the way out, reads it with `req.Value`. `Errors` then
returns nothing: a returned error would make the server write its own
500 page over the JSON answer.

<!-- file: src/middleware.zl -->
```zell title="src/middleware.zl"
// Middleware: code that runs around every handler. A middleware is a
// func(next Handler) Handler, so they compose by wrapping, and the
// router applies them in the order of Use (the first one is outermost).
namespace bookshelf.api

import net

import bookshelf.model

// AccessLog collects one line per request. A real service writes to the
// log package (http.Logging does); collecting lines lets the demo print
// them at the end, in a stable order, after the server has stopped.
struct AccessLog {
    mu    task.Mutex
    lines []string

    async func Add(line string) {
        await this.mu.Lock()
        this.lines = append(this.lines, line)
        this.mu.Unlock()
    }

    func Lines() []string {
        return this.lines
    }
}

// Logging records method, path, final status and body size, plus the
// internal cause of a 500 that Errors left in the request's values. It is
// outside Errors, so it sees the status Errors chose.
func Logging(log *AccessLog) func(http.Handler) http.Handler {
    return func(next http.Handler) http.Handler {
        return http.Func(async func(req *http.Request, res *http.Response) void! {
            await next.Serve(req, res)?
            var line = fmt.Format(
                "{0} {1} -> {2} ({3} bytes)",
                req.Method,
                req.RequestURI(),
                res.Status,
                len(res.Body),
            )
            switch req.Value(causeKey) {
                case string cause: line += " cause=" + strconv.Quote(cause)
            }
            await log.Add(line)
        })
    }
}

// The request-scoped value through which Errors hands the cause of a 500
// to Logging. req.Set/req.Value live as long as the request.
const causeKey = "bookshelf.cause"

// Errors turns handler errors into JSON answers. This is the one place
// that knows the HTTP meaning of the domain errors:
//   ValidationError -> 400 with the field, BadRequest -> 400,
//   anything wrapping ErrBookNotFound -> 404, everything else -> 500 with
//   a generic message: internal details go to the log, not to the client.
// errors.As and errors.Is search the whole chain, so a handler may wrap
// errors with ?("context") without breaking the mapping.
func Errors() func(http.Handler) http.Handler {
    return func(next http.Handler) http.Handler {
        return http.Func(
            async func(req *http.Request, res *http.Response) void! {
                try await next.Serve(req, res) catch err {
                    var (status, body) = classify(err)
                    if status == 500 {
                        req.Set(causeKey, err.Message())
                    }
                    // Handled: returning nothing tells the server the answer
                    // is complete, so it does not add its own 500 page.
                    res.SendJSON(status, body)
                }
            },
        )
    }
}

func classify(err error) (int, ErrorBody) {
    var ve ValidationError
    if errors.As(err, &ve) {
        return (400, ErrorBody{Error: ve.Message(), Field: ve.Field})
    }
    var br BadRequest
    if errors.As(err, &br) {
        return (400, ErrorBody{Error: br.Text})
    }
    if errors.Is(err, ErrBookNotFound) {
        return (404, ErrorBody{Error: err.Message()})
    }
    return (500, ErrorBody{Error: "internal error"})
}

// RequestID numbers requests and echoes the number in a header, the
// minimal version of the correlation id that ties log lines to requests.
func RequestID() func(http.Handler) http.Handler {
    var n = new sync.AtomicInt{}
    return func(next http.Handler) http.Handler {
        return http.Func(async func(req *http.Request, res *http.Response) void! {
            res.SetHeader("X-Request-Id", strconv.Itoa(n.Add(1)))
            await next.Serve(req, res)?
        })
    }
}
```

## src/main.zl: serve, call, shut down

`Addr: "127.0.0.1:0"` lets the operating system pick a free port, and
`srv.URL()` says which. `srv.Serve()` returns a task that runs until
shutdown; `main` keeps it and awaits it at the end.

The client side uses `http.NewRequest(...).Body(...).Send()` and, for the
typed call, `res.JSON<Book>()`: the same struct decodes the answer that
encoded it.

Graceful shutdown: `srv.Shutdown()` stops accepting connections and waits
for the requests in progress. The demo starts a slow request first, so
the output shows it completing with 200 after shutdown began.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// bookshelf: a JSON API over HTTP. main starts the server on a free
// loopback port, drives it with an HTTP client like a test suite would,
// shuts it down gracefully and prints the access log.
namespace bookshelf

import io
import net

import bookshelf.api
import bookshelf.model

// call sends one request and prints the status and body; the JSON bodies
// are printed as the client receives them.
async func call(base string, method string, path string, body string) void! {
    var req = http.NewRequest(method, base + path)
    if body != "" {
        req.Body(body).ContentType("application/json")
    }
    var res = await req.Send()?
    var line = fmt.Format("{0} {1} -> {2}", method, path, res.Status)
    if res.Header("Location") != "" {
        line += " Location: " + res.Header("Location")
    }
    println(line)
    if res.Body != "" {
        println("   ", strings.TrimRight(res.Body, "\n"))
    }
}

async func main() void! {
    var log = new AccessLog{}
    var repo = MemoryRepo.New()
    var r = api.Routes(repo)
    // Outermost first: every request gets an id, is logged, and its
    // errors are mapped before the log sees the final status.
    r.Use(api.RequestID())
    r.Use(api.Logging(log))
    r.Use(api.Errors())
    // An endpoint that takes a while, to show that shutdown waits for
    // requests already in progress.
    r.Get("/books/slow/report", async func(req *http.Request, res *http.Response) void! {
        await task.Delay(50)
        res.Text(200, "report done\n")
    })

    // A handler failing for a reason the client cannot fix: the Errors
    // middleware answers 500 without leaking the cause.
    r.Get("/admin/export", async func(req *http.Request, res *http.Response) void! {
        return error("export: disk quota exceeded")
    })

    // Port 0: the OS picks a free port, so the demo never collides with
    // anything already listening. A deployed service uses a fixed
    // address from its configuration, e.g. "0.0.0.0:8080".
    var srv = http.Server{Addr: "127.0.0.1:0", Handler: r}
    srv.Listen()?
    var serving = srv.Serve()
    var base = srv.URL()

    await call(
        base,
        "POST",
        "/books",
        "{\"title\": \"Dune\", \"author\": \"Frank Herbert\", \"year\": 1965, \"tags\": [\"sf\"]}",
    )?
    await call(
        base,
        "POST",
        "/books",
        "{\"title\": \"Emma\", \"author\": \"Jane Austen\", \"year\": 1815}",
    )?
    await call(
        base,
        "POST",
        "/books",
        "{\"title\": \"Persuasion\", \"author\": \"Jane Austen\", \"year\": 1817}",
    )?
    await call(base, "GET", "/books", "")?
    await call(base, "GET", "/books?author=austen", "")?
    await call(base, "GET", "/books/2", "")?
    await call(
        base,
        "PUT",
        "/books/2",
        "{\"title\": \"Emma\", \"author\": \"Jane Austen\", \"year\": 1815, \"tags\": [\"classic\"]}",
    )?
    await call(base, "DELETE", "/books/3", "")?
    await call(base, "GET", "/books/3", "")?
    await call(base, "GET", "/books/abc", "")?
    await call(base, "POST", "/books", "{\"title\": \"\", \"author\": \"Nobody\", \"year\": 2000}")?
    await call(
        base,
        "POST",
        "/books",
        "{\"title\": \"Future\", \"author\": \"Someone\", \"year\": 3000}",
    )?
    await call(base, "POST", "/books", "{not json")?
    await call(base, "GET", "/admin/export", "")?

    // Typed client side: the same Book struct decodes the answer.
    var res = await http.Get(base + "/books/1")?
    var dune = res.JSON<Book>()?
    println(
        "typed:",
        dune.Title,
        "by",
        dune.Author,
        dune.Year,
        dune.Tags,
        "request",
        res.Header("X-Request-Id"),
    )

    // Graceful shutdown: stop accepting, let the in-flight request finish,
    // then return. A real server waits for a signal first:
    //     await os.signal.Wait(os.signal.SIGTERM)
    var slow = http.Get(base + "/books/slow/report")
    await task.Delay(10)
    await srv.Shutdown()
    var report = await slow?
    print("in flight during shutdown: ", report.Status, " ", report.Body)
    await serving?
    println("server stopped")

    println("--- access log")
    for l in log.Lines() {
        println(l)
    }
}
```

## src/api_test.zl: tests without a network

`http.test.NewRequest` builds a request and `http.test.Serve` runs it
through the router and middleware into a recorder, in the same process,
with no socket. Each test builds a fresh repository, so tests do not
depend on each other. See [Testing](../../guide/testing.md).

<!-- file: src/api_test.zl -->
```zell title="src/api_test.zl"
// Handler tests without a network: net.http.test.NewRequest builds a
// request, and test.Serve runs it through the router and middleware into
// a recorder. Each test gets a fresh repository.
namespace bookshelf.tests

import net

import bookshelf.api
import bookshelf.model

func handler() http.Handler {
    var r = Routes(MemoryRepo.New())
    r.Use(Errors())
    return r
}

@test
async func TestCreateThenGet(t *testing.T) {
    var h = handler()
    var created = await http.test.Serve(
        h,
        http.test.NewRequest(
            "POST",
            "/books",
            "{\"title\": \"Emma\", \"author\": \"Jane Austen\", \"year\": 1815}",
        ),
    )
    testing.Equal(t, created.Status, 201)
    testing.Equal(t, created.Header("Location"), "/books/1")
    var got = await http.test.Serve(h, http.test.NewRequest("GET", "/books/1", ""))
    var b = try got.JSON<Book>() catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, b.Title, "Emma")
}

@test
async func TestErrorsMapToStatuses(t *testing.T) {
    var h = handler()
    var cases = [
        ["GET", "/books/9", "", "404"],
        ["GET", "/books/nine", "", "400"],
        ["POST", "/books", "{\"title\": \"x\", \"author\": \"\", \"year\": 1900}", "400"],
        ["POST", "/books", "[]", "400"],
        ["DELETE", "/books/1", "", "404"],
    ]
    for c in cases {
        var res = await http.test.Serve(h, http.test.NewRequest(c[0], c[1], c[2]))
        if strconv.Itoa(res.Status) != c[3] {
            t.Error(fmt.Format("{0} {1}: status {2}, want {3}", c[0], c[1], res.Status, c[3]))
        }
    }
}

@test
async func TestValidationNamesTheField(t *testing.T) {
    var res = await http.test.Serve(
        handler(),
        http.test.NewRequest(
            "POST",
            "/books",
            "{\"title\": \"T\", \"author\": \"A\", \"year\": 99}",
        ),
    )
    var body = try res.JSON<ErrorBody>() catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, body.Field, "year")
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
POST /books -> 201 Location: /books/1
    {"id":1,"title":"Dune","author":"Frank Herbert","year":1965,"tags":["sf"]}
POST /books -> 201 Location: /books/2
    {"id":2,"title":"Emma","author":"Jane Austen","year":1815}
POST /books -> 201 Location: /books/3
    {"id":3,"title":"Persuasion","author":"Jane Austen","year":1817}
GET /books -> 200
    [{"id":1,"title":"Dune","author":"Frank Herbert","year":1965,"tags":["sf"]},{"id":2,"title":"Emma","author":"Jane Austen","year":1815},{"id":3,"title":"Persuasion","author":"Jane Austen","year":1817}]
GET /books?author=austen -> 200
    [{"id":2,"title":"Emma","author":"Jane Austen","year":1815},{"id":3,"title":"Persuasion","author":"Jane Austen","year":1817}]
GET /books/2 -> 200
    {"id":2,"title":"Emma","author":"Jane Austen","year":1815}
PUT /books/2 -> 200
    {"id":2,"title":"Emma","author":"Jane Austen","year":1815,"tags":["classic"]}
DELETE /books/3 -> 204
GET /books/3 -> 404
    {"error":"book 3: not found"}
GET /books/abc -> 400
    {"error":"id must be a number, got \"abc\""}
POST /books -> 400
    {"error":"title is required","field":"title"}
POST /books -> 400
    {"error":"year 3000 is not a plausible year","field":"year"}
POST /books -> 400
    {"error":"invalid JSON: json: invalid character 'n' at offset 1"}
GET /admin/export -> 500
    {"error":"internal error"}
typed: Dune by Frank Herbert 1965 [sf] request 15
in flight during shutdown: 200 report done
server stopped
--- access log
POST /books -> 201 (74 bytes)
POST /books -> 201 (58 bytes)
POST /books -> 201 (64 bytes)
GET /books -> 200 (200 bytes)
GET /books?author=austen -> 200 (125 bytes)
GET /books/2 -> 200 (58 bytes)
PUT /books/2 -> 200 (77 bytes)
DELETE /books/3 -> 204 (0 bytes)
GET /books/3 -> 404 (29 bytes)
GET /books/abc -> 400 (44 bytes)
POST /books -> 400 (45 bytes)
POST /books -> 400 (60 bytes)
POST /books -> 400 (65 bytes)
GET /admin/export -> 500 (26 bytes) cause="export: disk quota exceeded"
GET /books/1 -> 200 (74 bytes)
GET /books/slow/report -> 200 (12 bytes)
```

The 404 message comes from the error chain (`book 3: not found`): the
repository wrapped the sentinel with context, and `errors.Is` still found
it. The 500 answer says only "internal error", while the access log has
the cause. Request 15 is the typed call: the first fourteen requests went
before it.

## Next

- [inventory-db](../inventory-db/README.md): a repository on SQLite.
- [Async](../../guide/async.md): tasks, mutexes and cancellation.
