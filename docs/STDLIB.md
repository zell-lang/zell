# Стандартная библиотека: карта Go → Zell

Все пакеты стандартной библиотеки Go, их место в Zell, статус и чем
версия Zell лучше. Уровни — порядок реализации по зависимостям: пакет
уровня N зависит только от уровней < N. Внутри уровня порядок свободный.

Статусы: ✅ есть — реализовано и покрыто тестами; 🟡 частично — есть ядро;
🔧 в работе; ❌ нет — не начато; ⛔ не нужно — сознательно не переносится.

Общие улучшения относительно Go, которые касаются всех пакетов:
- ошибки `T!` + `?` вместо `(T, error)`; типизированные sentinel-ошибки с
  `errors.Is/As` и трассой;
- generics везде, где Go использовал `interface{}` или кодогенерацию
  (`sort`, `container`, `sync.Map`, `heap`);
- `async` для всего ввода-вывода, без потока на соединение;
- атрибуты вместо строковых тегов (`@json(name: "id")`, проверяются
  компилятором);
- compile-time рефлексия для сериализации (скорость ручного кода);
- шаблоны логов/форматов проверяются при компиляции;
- пространства имён с полным путём вместо коротких пакетов.

## Уровень 0 — без зависимостей

| Go | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| `builtin` (len, append, panic…) | встроенные | ✅ есть | `?`, `try`, generics |
| `errors` | `errors` | ✅ есть | `Trace`/`Frames`, типизированные ошибки, `As<T>` |
| `cmp` | `Ordered`/`Comparable` constraints, `cmp` | ✅ есть (`Compare`, `Less`, `Min`, `Max`, `Clamp`, `Or`; NaN как в Go) | встроенные ограничения |
| `math` | `math` | ✅ есть | int-варианты (`IMin`), константы `MaxInt` |
| `math/bits` | `math.bits` | ✅ есть (`LeadingZeros/TrailingZeros/OnesCount/Len/RotateLeft/Reverse/ReverseBytes` (+`32`), `Add64/Sub64/Mul64/Div64` кортежами, `Add32/Mul32`; C-builtins) | кортежи вместо пар результатов |
| `math/cmplx` | `math.complex` | ✅ есть (`Complex{Re, Im}`: `Add/Sub/Mul/Div/Neg/Conj/Abs/Phase/Scale/String`; `Exp/Log/Sqrt/Pow/Sin/Cos/Polar/Rect/IsNaN/IsInf`) | методы с именами интерфейсов операторов (операторы — когда появятся) |
| `math/big` | `math.big` | ✅ есть (`Int`: арифметика и операторы `+ - * / % < ==` (`/` усекает как `int`, евклидово — `DivEuclid`/`Mod`), `Quo/Rem`, `Exp`, `ModInverse`, `GCD`, `Sqrt`, `Lsh/Rsh`, `ProbablyPrime`, `Text/ParseInt` 2..36, `Bytes/FromBytes`; `Rat`; `Decimal` с 7 режимами округления (`/` и `%` дают `Decimal!`, `DivRound(y, scale, mode)`); лимбы на C: Karatsuba, Knuth D); `Float` не планируется: точные вычисления — `Rat` и `Decimal` (деньги, фиксированная точность с явным округлением), приближённые — `float`; двоичная плавающая точка произвольной точности (`big.Float` Go: точность в битах, режимы округления IEEE) нужна редко и не окупает реализацию | неизменяемые значения, `Decimal` в stdlib, методы с именами интерфейсов операторов |
| `unicode`, `unicode/utf8`, `unicode/utf16` | `unicode`, `unicode.utf8/utf16` | ✅ есть (полные таблицы Unicode 15 из Go: `Is*`, `ToUpper/ToLower/ToTitle`, `SimpleFold`; utf8 `RuneLen/EncodeRune/AppendRune/DecodeRune*/Valid*/RuneCount*/FullRune`; utf16 `Encode/Decode/EncodeRune/DecodeRune`); нет нормализации NFC/NFD | генератор таблиц `scripts/gen-unicode-tables.py` |
| `strings` | `strings` | ✅ есть (`Contains`, `ContainsAny`, `Index`, `IndexAny`, `Replace` — каждое вхождение, `ReplaceAll` — то же под именем Go, …; `Builder` с `Write`/`WriteString`/`WriteByte`/`WriteRune`) | `Builder`, `Runes`, `Reverse`, `Pad*`, `Title` |
| `bytes` | `bytes` | ✅ есть (`Buffer` + функции `strings` над `[]byte`) | `Buffer` реализует `io.Reader/Writer` |
| `strconv` | `strconv` | ✅ есть (`ParseInt/ParseUint` с базами и `_`, `*Bits`, `ParseFloat`, `ParseBool`, `Format*`, `Itoa/Atoi`, `Quote/QuoteToASCII/QuoteRune/Unquote`, `Parse<T>`/`Format<T>`) | generic `Parse<T>`/`Format<T>`, `errors.Is(err, strconv.ErrSyntax)` |
| `slices` | `slices` | ✅ есть (`Sort/SortBy/SortFunc` стабильные, `BinarySearch*`, `Index*/Contains*`, `Map/Filter/Reduce`, `Unique/Compact`, `Chunk/Window/Zip/Flatten/GroupBy`, `Min/Max/Sum`, `Equal/Compare`, `Insert/Delete`, `Fill/Repeat/Shuffle`) | `Map`, `Filter`, `Reduce`, `Chunk`, `Window`, `Unique`, `Zip` generic |
| `maps` | `maps` | ✅ есть (`Keys`, `Values`, `SortedKeys/Values`, `Merge`, `Copy`, `Invert/InvertAll`, `Filter`, `DeleteFunc`, `MapValues`, `Equal*`, `Clone`) | `SortedKeys`, `Invert` |
| `sort` | `slices.Sort*` | ✅ есть | generic, стабильная сортировка по умолчанию, `SortBy(key)` |
| `container/list`, `ring`, `heap` | `collections` (`List`, `Map`, `Set`, `Queue`, `Stack`) + `LinkedList`, `Ring`, `Heap<T>`, `PriorityQueue<T>`, `Deque<T>`, `OrderedMap`, `LRU`, `BitSet` | ✅ есть (`for x in c` по протоколу итерации: `Iter()` у `LinkedList`/`OrderedMap`/`LRU`, `Items()` у остальных; `BitSet` с `b[i]`) | generic, без `any` |
| `iter` | `for x in` + `iter.Seq<T>` интерфейс | ❌ нет | ленивые генераторы через async? — нет; итератор-интерфейс |
| `hash`, `hash/fnv`, `crc32`, `crc64`, `adler32`, `maphash` | `hash` (+ `.fnv`, `.crc32`, `.crc64`, `.adler32`, `.maphash`) | ✅ есть (интерфейсы `Hasher/Hash32/Hash64`, `hash.Combine`; maphash — SipHash-1-3 на C) | — |
| `encoding/binary` | `encoding.binary` | ✅ есть (`BigEndian/LittleEndian.*`, varint, `Encode/Decode/Read/Write<T>` через compile-time рефлексию, `@binary(endian:)`) | `Read<T>`/`Write<T>` структур через compile-time рефлексию, `@binary(endian)` |
| `time` (типы) | `time` | ✅ есть (`Duration`, `Time` с раскладками Go и strftime, `ParseTime`, `Location` из tzdata + POSIX-правила, `Instant`, `Ticker/Timer` async); имена `Now/Since/Sleep/Parse` заняты старыми int-функциями: `Time.Now()`, `Elapsed`, `SleepFor`, `ParseTime` | типы `Time`, `Duration`, `Instant` монотонный |
| `unsafe` | `unsafe` (только FFI) | 🟡 частично (`ReadInt/ReadUint/ReadFloat/WriteInt/ReadBytes/WriteBytes/CString/AddrOf/TypeOf/CallFunc`; импорт только с `[capabilities] unsafe = true`, ABI.md §22.5) | минимальный, помечает capability |
| `expr` | — (C# `System.Linq.Expressions`) | ✅ есть: деревья выражений `expr<F>` (`Expr<F>`: `Root`, `Params`, `ParamNames`, `Compile`, `String`), узлы `Node` (`Param`, `Field`, `Const`, `Captured`, `Binary`, `Unary`, `Call`, `IsNil`, `Tuple`, `Struct`), `Op`, `TypeOf`, `FieldOf` (дескриптор поля с атрибутами), `Children`, `Walk`, `Values`, `Format`, комбинаторы `And`/`Or`/`AndAll`/`OrAll`/`Not`; DESIGN.md «Деревья выражений» | структура узлов — константа в rodata, построение дерева выделяет не больше одного блока захваченных значений |
| `reflect` | `reflect` | ✅ есть (статические таблицы `TypeInfo`/`FieldInfo`/`MethodInfo`/`ParamInfo`/`VariantInfo`, `TypeOf<T>`, `typeof`, `ValueOf`, `Fields<T>()` с развёртыванием при компиляции, статический `if`, атрибуты как значения, `Call<F>`/`Invoke`, `New`, `TypeInfo.Package`, `MethodInfo.Params`; `TypesWith<A>()` — типы всей программы с атрибутом, константный срез; `Error(msg)` — ошибка компиляции из библиотеки; DESIGN.md «Рефлексия») | быстрее C#: указатели на функции, без boxing в `Call<F>`; таблицы урезаются, если `reflect` не используется |
| `weak`, `unique` | `weak *T` в языке; `intern` | ✅ есть (weak) / нет | язык |

## Уровень 1 — базовый ввод-вывод, ОС, синхронизация

| Go | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| `io` | `io` (`Reader`, `Writer`, `Closer`, `Seeker`, `Copy`, `ReadAllFrom`, `LimitReader`, `MultiWriter`, `Pipe`, `BoundedPipe`, `TeeReader`) | ✅ есть (+ `AsyncReader/AsyncWriter`, их реализует `net.Conn`; `os.File`) | интерфейсы async-совместимые (`AsyncReader`); нет `SendFile` в `Copy` |
| `bufio` | `io.Buffered*`, `Scanner` | ✅ есть (`ReadLine/ReadString/ReadBytes/Peek/ReadRune`, `Flush`, `Scanner` с `ScanLines/Words/Runes/Bytes`, `io.Lines`/`Words`) | `Lines()`/`Words()` (срезами, итераторов нет) |
| `fmt` | `fmt` | ✅ есть (Format с выравниванием `{0,5}`/`{0,-5}` как в C#, Sprintf) | `{0}`-шаблоны проверяются при компиляции, `Stringer` интерфейс |
| `os` | `os` | ✅ есть (args, `Getenv`/`Setenv`/`Environ` (все переменные как `KEY=value`, с учётом `Setenv`), exit, cwd, hostname…) | константы платформы, `OnInterrupt` |
| `os/signal` | `os.signal` | ✅ есть (`Wait`, `WaitAny`, `Notify` → `Channel<int>`, `Catch`, `Ignore`, `Reset`, `Raise`; self-pipe + поток) | async `await signal.Wait(SIGTERM)` |
| `os/exec` | `process` | ✅ есть (`Run/RunWith/Output`, `Start` с пайпами `AsyncReader/AsyncWriter`, `Wait/Kill/Signal`, env/dir, `Timeout` и отмена задачи, `LookPath`; pidfd) | async `Run`, пайпы как `Reader/Writer`, таймауты через контекст |
| `os/user` | `os.user` | ✅ есть (`Current`, `Lookup`, `LookupId`, `LookupGroup(Id)`) | — |
| `io/fs`, `path`, `path/filepath` | `fs` (+ `fs.Path` функции) | ✅ есть | `Walk` async, glob, `Watch` (inotify/FSEvents) |
| `embed` | `@embed("file")` атрибут | ✅ есть (`string`, `[]byte`, `const`, `embed.FS` из каталога или `dir/*.ext`) | проверяется при компиляции, `embed.FS` |
| `flag` | `cli` | ✅ есть (подкоманды, флаги из полей структуры `@cli.flag(...)` (умолчание — значение поля или `default: "текст"`), env fallback, help, «did you mean», completion bash/zsh/fish; ошибка команды с `ExitCode() int` (`cli.ExitCoder`) задаёт код выхода `App.Run`, в том числе через обёртки `?("ctx")`; `App.Out` — куда писать help и версию (по умолчанию stdout), `App.Errors` — сообщения об ошибках (stderr)) | подкоманды, help из атрибутов, env fallback |
| `sync`, `sync/atomic` | `task` (`Mutex`, `WaitGroup`) + `sync` (`RwMutex`, `Semaphore`, `Barrier`, `Once`, `AtomicInt/AtomicBool`, `Atomic<T>`, `Pool<T>`) | ✅ есть | async-aware мьютексы, generic атомики |
| `context` | контекст задачи в `task` | ✅ есть | неявная передача, типизированные ключи, отмена дерева |
| `math/rand`, `crypto/rand` | `random`, `crypto.rand` | ✅ есть | — |
| `crypto/*` примитивы | `crypto.{hash,hmac,aes,chacha,x25519,ecdsa,ed25519,rsa,hkdf,kdf,password}` | ✅ есть hash (+ BLAKE2b), hmac, aes-gcm, aes-cbc + PKCS#7, chacha, x25519, P-256 и P-384 (ECDH/ECDSA, RFC 6979), ed25519, rsa (подпись, OAEP, генерация ключей), hkdf, kdf (PBKDF2, scrypt, Argon2d/i/id по RFC 9106), password (PHC `$argon2id$`, `Verify`, `NeedsRehash`); нет P-521, SHA-3 | `password.Hash/Verify` на Argon2 из коробки |
| `encoding/base64`, `base32`, `hex`, `ascii85`, `pem`, `asn1` | `encoding.*` | 🟡 есть base64, hex, base32, pem (с заголовками), asn1 (DER reader/Builder поверх ридера x509); нет ascii85 | — |
| `runtime` | `runtime` (`NumCPU`, `Version`, `GC` → нет) | 🟡 частично (`diag.Runtime()` с `HeapInUse`/`HeapRetained`/`HeapReleased`, `runtime.FreeOSMemory()`) | — |
| `testing` | `testing` + `zell test` | ✅ есть (`@test` с `*testing.T`/без параметров, бенчмарки `@test func B(b *testing.B)`, `t.Run`, `Fatal/Error/Skip/Log`, assert-функции (`Equal`/`EqualMsg`/`True(t, cond, msg)`, …), `--filter/--json/--bench/-v/--parallel`, тест в отдельном процессе) | `@test`, табличные тесты, assert; нет snapshot |
| `log` | `log` | ✅ есть | уровень Serilog: шаблоны при компиляции, sinks, enrichers |
| `log/slog` | то же | ✅ есть | — |
| `log/syslog` | `log.WriteTo.Syslog` | ❌ нет | — |
| `expvar` | `diag` метрики | ✅ есть | Prometheus/OTLP |
| `runtime/pprof`, `runtime/trace`, `runtime/debug` | `diag` | 🟡 частично (cpu, heap) | pprof-совместимо, спаны OTel |

## Уровень 2 — форматы, текст, сжатие, сеть (ядро)

| Go | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| `encoding/json` | `encoding.json` | ✅ есть (Parse<T>/Format/FormatIndent (как `MarshalIndent`)/Value; `time.Time` — строка RFC 3339, `time.Duration` — наносекунды, как Go; `json.Value`, `yaml.Value` и `toml.Value` делят `IsObject()`/`IsArray()`) | compile-time, пути в ошибках, атрибуты |
| `encoding/xml` | `encoding.xml` | ✅ есть (pull-`Reader` с пространствами имён, дерево `Value`, `Writer`, `Parse<T>/Decode<T>/Format<T>` с `@xml(name, attr, text, ns, omitEmpty, skip)`; C-токенизатор; DTD не раскрывается) | та же модель, pull-reader, атрибуты вместо тегов |
| — (yaml, toml в Go нет) | `encoding.yaml`, `toml` | ✅ есть: yaml (YAML 1.2 core: блоки, flow, якоря/алиасы с лимитом, merge `<<`, теги, многодокументность; `Value`, `Parse<T>/Format<T>` с `@yaml`; сверено с PyYAML), toml (TOML 1.0 полностью, даты, `Parse<T>/Format<T>` с `@toml`; `Document` правит текст с сохранением комментариев и порядка; сверено с tomllib) | в стандартной библиотеке, round-trip с комментариями (toml `Document`; в yaml комментарии не сохраняются) |
| `encoding/csv` | `encoding.csv` | ✅ есть (`Reader`/`Writer` как в Go, `LazyQuotes`, `FieldsPerRecord`, `Read<T>/ReadNoHeader<T>/Write<T>` с `@csv(name, index, skip, omitEmpty)`; сверено с Go) | `Read<T>`/`Write<T>` в структуры |
| `encoding/gob` | `encoding.binary` схема-типизированный | ❌ нет | — |
| `regexp`, `regexp/syntax` | `regex` | ✅ есть (синтаксис RE2, Unicode-классы и скрипты, ленивый DFA + Pike VM, линейное время, без ReDoS; `Find/FindAll/Iter/Groups/Replace/ReplaceFunc/Split`; `regex.Pattern("...")` проверяется при компиляции; сверено с Go на 336 случаях) | литералы проверяются при компиляции, именованные группы, `Iter`; в 7 раз быстрее Go на `FindAll` |
| `text/template`, `html/template` | `text.template`, `html.template` | ✅ есть (`Parse<T>("литерал")` — поля, функции и `{{end}}` проверяются при компиляции; pipelines, `range/with/if/define/template/block`, встроенные функции Go; html — контекстное автоэкранирование HTML/атрибутов/URL/JS/CSS, `HTML/URL/JS` безопасные типы; сверено с Go) | типизированные шаблоны (проверка полей при компиляции), автоэкранирование |
| `text/scanner`, `text/tabwriter` | `text.scanner`, `text.table` | ✅ есть (scanner — токены и сообщения об ошибках как в Go, сверено с Go; table — стили Plain/ASCII/Unicode/Markdown, выравнивание, ширина CJK/комбинирующих символов, многострочные ячейки) | таблицы с рамками и Markdown вместо `tabwriter` |
| `html` | `html` (escape/unescape) | ✅ есть (`EscapeString/UnescapeString`, все 2231 сущности HTML5; сверено с Go) | — |
| `mime`, `mime/multipart`, `mime/quotedprintable` | `mime` | ✅ есть (`TypeByExtension/ExtensionsByType`, `ParseMediaType` с RFC 2231, `EncodeWord/DecodeHeader`; `mime.multipart` — `Reader/Writer`, `ReadForm`; `mime.quotedprintable`; сверено с Go) | — |
| `compress/gzip`, `zlib`, `flate`, `bzip2`, `lzw` + zstd, brotli | `compress.{gzip,zlib,deflate,zstd,brotli}` | 🟡 есть deflate/zlib/gzip (кодер и декодер на C, уровни 0–9, потоковые `Writer/Reader` по кускам `[]byte` и адаптеры `io`: `gzip.NewStreamReader/NewStreamWriter`), zstd: декодер (RFC 8878, XXH64) и кодер (`zstd.Compress(data, level)`: хеш-цепочки, Huffman-литералы, FSE-таблицы последовательностей, repeat offsets; кадры читает `zstd -d`; степень сжатия около эталонных уровней 1–3), декодер brotli (RFC 7932: словарь и 121 преобразование, все режимы контекста; словарь хранится сжатым zstd и разворачивается при первом использовании; сверено с CLI `brotli` на уровнях 0–11 и окнах 10–24); нет кодера brotli, bzip2, lzw | zstd из коробки; gzip быстрее Go в 1.4 раза (сжатие) и в 3 раза (распаковка) |
| `archive/zip`, `archive/tar` | `archive.{zip,tar}` | ✅ есть (zip: чтение/запись, Zip64, deflate/store, расширенные метки времени, `ExtractTo` с защитой от zip-slip, `FromDir`; tar: ustar/pax/GNU, длинные имена, base-256, ссылки; совместимо с zip(1)/unzip/tar) | защита от zip-slip по умолчанию |
| `net` (TCP/UDP/Unix/DNS/IP) | `net` | ✅ есть; в работе (закрытие пробелов) | async без потока на соединение, типизированные ошибки, Happy Eyeballs |
| `net/netip` | `net.IP`, `net.Addr` | 🔧 в работе | — |
| `net/url` | `net.url` | ✅ есть (`Parse/ParseRequestURI`, `URL` со всеми полями Go, `ResolveReference`, `JoinPath`, `Values`, `ParseQuery`, `QueryEscape/PathEscape`, IPv6 и зоны; сверено с Go). `http.ParseURL` пока отдельный | — |
| `net/textproto` | `net.textproto` | ✅ есть (`Header` с каноническими ключами, `ReadHeader` с продолжениями строк, `DotEncode/DotDecode`, `ReadResponse` многострочных ответов; на строках, без `Reader`) | — |
| `image`, `image/png`, `jpeg`, `gif`, `color`, `draw` | `image` | ❌ нет | + webp, avif декодеры позже |
| `index/suffixarray` | `text.suffixarray` | ⛔ не нужно (пакет) | — |
| `database/sql`, `sql/driver` | `database.sql` + драйверы `database.postgres` (на Zell), `database.sqlite` (системная libsqlite3) | ✅ есть (интерфейс драйвера `Driver/DriverConn/DriverStmt/DriverRows/DriverTx`, `sql.Register/Open/OpenDriver`, асинхронный пул: max open/idle, время простоя и жизни, health check, ожидание с отменой; `Exec`, `QueryRows`, `QueryRow`, `Query<T>`/`QueryOne<T>` в структуры через compile-time рефлексию с `@sql.column`, `Null<T>`, указатели для NULL, `time.Time`, `big.Decimal`, `json.Value`; именованные `:name` → `$1`/`?`; транзакции, `Transact`, `Prepare` с кэшем подготовленных выражений на соединение, `Conn`, `Raw`; отмена и таймауты через контекст задачи; PostgreSQL: протокол 3.0, trust/password/MD5/SCRAM-SHA-256, sslmode до verify-full, simple и extended протоколы, бинарные результаты, COPY, уведомления; проверено на PostgreSQL 16) | async, `Query<T>` в структуры, пул, `Query<T>` вместо `Scan` |
| Entity Framework Core (EF Core), `gorm`/`ent` | `data` (станет пакетом `zell.data`) | ✅ есть (модель из атрибутов `@table/@key/@column/@index/@hasMany/@belongsTo/@manyToMany/@concurrencyToken/...`, соглашений и `Configure`; `data.Open<Ctx>` с `data.Set<T>`; запросы `Where/OrderBy/Select/Include/GroupBy/Join/Sum/...` с лямбдами-деревьями выражений (`expr<F>`), переведёнными в SQL библиотекой (кэш по месту лямбды; предикаты в переменных, из функций, `expr.OrAll`), `ToSql`; change tracking со снимками и UPDATE только изменённых столбцов, карта идентичности, единица работы, пакетные INSERT с `RETURNING`, upsert `ON CONFLICT`, оптимистичная блокировка, транзакции, `AsNoTracking`, явная загрузка, `ExecuteUpdate/ExecuteDelete` (и `…Returning`), `Raw<T>`; расширенный SQL: подзапросы (`data.Exists/In/CountOf`, коррелированные через `data.Where`), `LeftJoin/RightJoin/FullJoin/CrossJoin`, `Union/Intersect/Except`, `AsCte`/`Recursive`, оконные функции (`data.RowNumber(data.Over().PartitionBy(...))`), `UpsertWith/InsertOrIgnore`, сырые фрагменты `data.Sql1<T>("lower(?)", x)`; PostgreSQL: `@data.jsonb` (`jsonb`, пути и операторы; nullable `*Meta`, `nil`-члены документа, массивы документов `[]Item` с `data.Any/All/Count` и `len`), массивы, `ILike`, регулярные выражения, полнотекстовый поиск, `DistinctOn`, `ForUpdate/SkipLocked`, `LISTEN/NOTIFY`, GIN; провайдеры — диалекты-плагины (`interface Dialect`, `data_sqlite.zl`, `data_postgres.zl`) PostgreSQL и SQLite; миграции как код: `zell data migration add/update/script/list`, `ctx.Migrate()`; см. FRAMEWORK.md §3) | лямбды не замыкания, а деревья выражений, которые библиотека переводит в SQL; модель из рефлексии, маппинг развёрнут по `reflect.Fields<T>()` — без поддержки в компиляторе |
| `go/*` (ast, parser, types…) | `zell.{ast,parser,types}` — экспорт компилятора как библиотеки | 🟡 частично (внутри компилятора) | — |
| `debug/elf`, `dwarf`, `pe`, `macho` | `debug.*` | ❌ нет (для `diag` частично) | — |
| `plugin` | `plugin` (dlopen) | 🟡 частично (внутренние `rtOpen/rtSym/rtClose`, capability `plugin`) | capability-помечен; используется `debug` |
| `runtime/debug` (частично), — | `debug` — отладочный агент на Zell (DAP, DWARF, точки, задачи) | ✅ есть (DAP-агент: точки, условия, логпоинты, шаги по задачам, исключения, переменные, выражения v1; без DWARF — по таблицам компилятора) + `thread` (`Spawn/Join/Sleep`) | см. `docs/DEBUGGER.md` этап 2 |

## Уровень 3 — протоколы и сервисы

| Go | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| `crypto/tls` | `net.tls` | ✅ есть (1.3 + 1.2 ECDHE/AEAD, группы X25519/P-256/P-384, подписи ECDSA P-256/P-384, interop с OpenSSL) | только современные наборы, без C-зависимостей |
| `crypto/x509`, `x509/pkix` | `crypto.x509` | ✅ есть (P-256 и P-384; `SystemRoots()` разбирается раз на процесс и переиспользуется, пока у файла те же путь, размер и mtime — каждый вызов получает свою копию пула; без OCSP/CRL, без P-521) | — |
| `net/http` | `net.http` | ✅ есть (HTTP/1.1 и HTTP/2 (h2 по ALPN, h2c) клиент/сервер; пул соединений с прокси, gzip, cookies, потоковые тела; роутер с middleware (Recover, Logging, CORS, Gzip, BodyLimit, TimeoutHandler), ServeFile/ServeDir с диапазонами и ETag, SSE; interop с curl и Go); нет HTTP/3, server push | async, роутер с параметрами и middleware в stdlib, graceful shutdown с GOAWAY, сервер на уровне Go `net/http` по скорости |
| `net/http/httptest`, `cookiejar` | `net.http.test`, `http.CookieJar` | ✅ есть (`NewServer`/`NewTLSServer`/`NewH2CServer`, `NewRecorder`, `NewRequest`, `Serve`; jar по RFC 6265) | тестовый сервер сразу с настроенным клиентом |
| `net/http/httputil`, `fcgi`, `cgi`, `pprof` | `net.http.util`, `diag` endpoints | ❌ нет | — |
| `golang.org/x/net/http2/hpack` | `net.http.hpack` | ✅ есть (RFC 7541, векторы приложения C) | — |
| — (websocket в Go нет) | `net.ws` | ✅ есть (RFC 6455: Upgrade/Dial, фрагментация, ping/pong, коды закрытия, `for msg in conn`; interop с Go и curl); нет permessage-deflate | в stdlib |
| `net/mail`, `net/smtp` | `net.mail` | ✅ есть (`Address`, `ParseAddress/ParseAddressList` с RFC 2047 и группами, `Message{From, To, Cc, Bcc, Subject, Text, HTML, Attachments}.Build()` — MIME multipart/mixed/alternative/related, quoted-printable, base64, кодирование и перенос заголовков; SMTP-клиент `Dial/Send/SendWith` со STARTTLS и SMTPS, AUTH PLAIN/LOGIN/CRAM-MD5, типизированные `SMTPError`); нет DKIM | `Send` в одну строку, TLS по умолчанию |
| `net/rpc`, `rpc/jsonrpc` | `zell.grpc`, `net.jsonrpc` | ❌ нет | gRPC вместо net/rpc |
| `testing/fstest`, `iotest`, `quick` | `testing.*` | ❌ нет | property-based `quick` с generics |
| `syscall` | `sys` (слой платформы) | 🟡 частично (внутри рантайма) | единый API на всех ОС |
| `time` (таймеры, тикеры) | `task.Delay`, `time.NewTicker/NewTimer/After` | ✅ есть | async (`await t.Tick()`) |

## Уровень 3б — клиенты инфраструктуры (в Go внешние библиотеки)

| Go (внешние) | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| `go-redis` | `data.redis` | ✅ есть (RESP2/RESP3 с откатом, `Connect(url)` с `redis://`/`rediss://`/`unix://`, AUTH, db; ~100 типизированных команд методами + `Do(args []any)`; `redis.Nil`, `redis.Error{Code}`; пул с PING-проверкой, сроком жизни, `ErrPoolTimeout`, повторами; `Pipeline()` одной записью (задачи команд завершаются после `Exec`), `TxPipeline` и `Watch` (`ErrTxFailed`), `NewScript().Run` с EVALSHA→EVAL; `Subscribe/PSubscribe` + `for msg in sub` с переподключением; таймауты через контекст задачи; Sentinel (`ConnectSentinel`: мастер, `+switch-master`, повторный опрос при READONLY, `ReadFromReplicas`); Cluster (`ConnectCluster`: `CLUSTER SLOTS`, hash tags, MOVED/ASK, `ErrCrossSlot`, широковещательные FLUSHALL/KEYS/DBSIZE, SCAN по всем мастерам); `GetJSON<T>/SetJSON<T>`. Проверено на redis-server 7.0: TLS + пароль, Sentinel с `kill -9` мастера → повышенная реплика, Cluster из 3 мастеров с миграцией слота (ASK, затем MOVED); spec-тесты против мок-серверов) | RESP3, pipelining, pub/sub, транзакции, Lua-скрипты, cluster, **Sentinel** с автоматическим failover, типизированные команды, async |
| `sarama`/`franz-go` | `data.kafka` | ✅ есть (протокол на Zell: ApiVersions, Metadata, Produce v7/v3, Fetch v10/v4, ListOffsets, FindCoordinator, JoinGroup/SyncGroup/Heartbeat/LeaveGroup, OffsetCommit/OffsetFetch, SASL PLAIN и SCRAM-SHA-256/512, CreateTopics/DeleteTopics, InitProducerId; record batch v2 побайтно как Java-клиент, CRC-32C на C, gzip/zstd; `NewProducer` с батчами по размеру и linger, acks, идемпотентностью, murmur2 как в Java, round-robin; `NewConsumer` с группами (range/round-robin, ребаланс, heartbeat в фоне, авто/ручной commit), `for rec in consumer`; `NewAdmin`; TLS; смена лидера и повторы с backoff. Проверено на Kafka 3.7 (KRaft, SASL PLAIN/SCRAM) — тест `--ignored` с `KAFKA_BROKERS`; spec-тесты против мок-брокера на Zell; нет snappy/lz4, транзакций, статического членства) | producer с батчами и idempotence, consumer groups с rebalance, SASL (PLAIN/SCRAM), TLS, компрессия gzip/zstd, транзакции позже; один клиент на Zell без C |
| `aws-sdk-go` S3 | `cloud.s3` | ✅ есть (SigV4 заголовками и presigned URL — векторы набора AWS и документации S3, сверено с botocore; `Config{Endpoint, Region, AccessKey, SecretKey, SessionToken, PathStyle}` для AWS/MinIO/R2/GCS; Put/Get (поток, диапазон)/Head/Delete/DeleteObjects, `ListObjectsV2` и итератор `List`, `CopyObject`, бакеты, multipart `Upload` с параллельными частями и abort, `Presign`, `s3.Error` + `ErrNoSuchKey/NoSuchBucket/AccessDenied/PreconditionFailed`, повторы 5xx/SlowDown; учётные данные: env, `~/.aws/credentials`, IMDSv2. Проверено на MinIO; spec-тест против S3-сервера на Zell, проверяющего подписи; тела запросов в памяти — потоковой загрузки одним PUT нет, только multipart) | SigV4, Get/Put/Delete/List, multipart upload с параллельными частями, presigned URLs, потоковые тела, совместимо с MinIO/R2/GCS-S3 |
| `minio-go`, `gocloud blob` | `cloud.blob` (абстракция над s3/fs) | ✅ есть (`Bucket`: Write/Read/Delete/Exists/Attributes/List/Copy/SignedURL; `OpenS3`, `OpenDir` с атомарной записью и защитой от `..`, `Open("s3://…"/"file://…")`, `blob.ErrNotFound`) | единый интерфейс для S3 и локальной ФС |

## Уровень 4 — фреймворк (`docs/FRAMEWORK.md`)

`zell.data`, `zell.log` (расширенные sinks), `zell.diag`
(endpoints, trace), `zell.config`, `zell.cache`, `zell.jobs`,
`zell.auth`, `zell.messaging`, `zell.cli`, `zell.test`.

| Аналог | Zell | Статус | Лучше чем в Go |
|---|---|---|---|
| ASP.NET Core, `gin`/`echo` + `wire` + `viper` + `golang-jwt` + `swag` | будущий пакет `zell-lang/web` | ⏸ вынесен из дерева; `reflect.TypesWith<A>()`, `MethodInfo.Params` и `reflect.Error` для него готовы (2026-10-24), пакет пишется (был в stdlib как `web`; исходники — тег git `web-in-tree`, последний тулчейн с ним — `v0.1.0-rc.3`); пока — `net.http` (роутер, middleware, HTTP/2, WebSocket) | атрибутная маршрутизация и DI без рефлексии в рантайме |

## Чего в Go нет, а в Zell будет в stdlib

`collections` (полный набор), `yaml`/`toml`, `regex` с проверкой при
компиляции, `websocket`, `zstd`/`brotli`, `uuid`, `password` (Argon2),
`decimal`/`big` с операторами, `expr` (деревья выражений), `log` уровня Serilog, `diag` с OTel и
Prometheus, `cli` с подкомандами, `arena`, типизированный `template`.

## План волн

1. **Уровень 0 + 1 пробелы**: `slices`, `maps`, `sort`, `bytes`, `strconv`,
   `time.Time/Duration`, `math.bits/big/complex`, `unicode`, `hash`,
   `encoding.binary`, `collections` (heap, deque, LRU, ordered map), `io`
   интерфейсы + `bufio`, `process`, `signal`, `cli`, `sync`, `testing`,
   `embed`, `base32/pem/asn1`.
2. **Уровень 2**: `regex`, `compress`, `archive`, `csv`, `xml`, `yaml`,
   `toml`, `template`, `html`, `mime`, `url`, `textproto`, `image` (png,
   jpeg).
3. **Уровень 3**: HTTP/2, пул соединений, websocket, `mail`, `sql` +
   PostgreSQL/SQLite, `httptest`, `debug/elf`.
4. **Уровень 4**: фреймворк.

Таблица обновляется при каждом изменении статуса.
