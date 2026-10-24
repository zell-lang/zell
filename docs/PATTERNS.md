# ООП и паттерны в Zell

В Zell нет наследования классов. Четыре механизма заменяют его:

1. **Структуры со встраиванием** — композиция и подъём методов.
2. **Структурные интерфейсы** — полиморфизм без объявления связи.
3. **Enum с данными + `switch`** — закрытые множества вариантов.
4. **Замыкания** — поведение как значение.

Плюс generics для обобщённых контейнеров и алгоритмов. Ниже — как на это
ложатся привычные паттерны и где кроются отличия от C#/Java.

## Встраивание — это композиция, не наследование

```zell
struct Base {
    Name string
    func Run() { println("run"); this.Step() }
    func Step() { println("base step") }
}

struct Derived {
    Base
    func Step() { println("derived step") }
}

var d = Derived{}
d.Run()      // печатает "run", затем "base step" — НЕ "derived step"
```

`Base.Run` вызывает `Base.Step`: виртуальной диспетчеризации через
встраивание нет (как в Go). Это сделано намеренно: поведение `Base` нельзя
сломать снаружи. Когда нужна подмена шага, используйте интерфейс:

```zell
interface Stepper { func Step() }

struct Runner {
    stepper Stepper
    func Run() { println("run"); this.stepper.Step() }
    static func New(s Stepper) Runner { return Runner{stepper: s} }
}

struct Loud { func Step() { println("LOUD") } }
Runner.New(Loud{}).Run()     // run, LOUD
```

Это и есть «шаблонный метод» по-слэнговски: изменяемая часть вынесена в
интерфейс и передаётся явно.

## Таблица паттернов

| Паттерн | Как в Zell |
|---|---|
| Стратегия | интерфейс с одним методом или значение `func(...)` |
| Наблюдатель | срез `[]func(Event)` или интерфейс `Observer`; `events.Hub<T>` в stdlib |
| Команда | замыкание или структура с методом `Execute()` |
| Состояние | enum состояний + `switch`, либо интерфейс `State` с переходами |
| Декоратор | структура, хранящая интерфейс и реализующая его же |
| Адаптер | структура-обёртка, реализующая целевой интерфейс |
| Прокси | то же, что декоратор, с ленивой инициализацией или кэшем |
| Фасад | пространство имён с несколькими функциями |
| Фабрика | `static func New(...)`; интерфейс `Factory<T>` при необходимости |
| Строитель | методы-цепочки на `*Builder`; functional options |
| Синглтон | глобальная `var` + `sync.Once`; чаще — DI-контейнер |
| Посетитель | enum с данными + исчерпывающий `switch` |
| Интерпретатор | enum-дерево + рекурсивный `switch` |
| Компоновщик | интерфейс `Node` + структура со срезом `[]Node` |
| Итератор | `for x in` поверх срезов/map/коллекций; интерфейс `Iterator<T>` |
| Цепочка обязанностей | срез обработчиков `[]Handler` или middleware `func(ctx, next)` |
| Внедрение зависимостей | поля-интерфейсы + конструктор; контейнер — в будущем пакете `zell-lang/web` |
| Repository / Unit of Work | интерфейсы + generics (`Repo<T>`), `zell.data` |

## Примеры

### Декоратор

```zell
struct Cached {
    inner Store
    cache map[string]string

    static func New(inner Store) Cached {
        return Cached{inner: inner, cache: map[string]string{}}
    }

    func Get(key string) string! {
        if this.cache.Has(key) {
            return this.cache[key]
        }
        var v = this.inner.Get(key)?
        this.cache[key] = v
        return v
    }

    func Set(key, value string) void! {
        this.inner.Set(key, value)?
        this.cache[key] = value
    }
}

var s Store = Cached.New(DiskStore{Path: "/tmp"})   // Cached тоже Store
```

### Строитель с functional options

```zell
struct Server {
    addr    string
    timeout int

    static func New(addr string, opts ...func(*Server)) Server {
        var s = Server{addr: addr, timeout: 30}
        for opt in opts { opt(&s) }
        return s
    }
}

func WithTimeout(sec int) func(*Server) {
    return (s *Server) => { s.timeout = sec }
}

var srv = Server.New(":8080", WithTimeout(10))
```

### Посетитель через enum

```zell
enum Expr {
    Num(value float)
    Add(left *Expr, right *Expr)
    Mul(left *Expr, right *Expr)
}

func Eval(e *Expr) float {
    switch *e {
        case Expr.Num(v):    return v
        case Expr.Add(l, r): return Eval(l) + Eval(r)
        case Expr.Mul(l, r): return Eval(l) * Eval(r)
    }
}
```

Один enum и одна функция вместо иерархии классов и интерфейса `Visitor`.
Новая операция — новая функция; новый вариант — компилятор укажет все
`switch`, где его нет (для `sealed` множеств, см. ниже).

### Состояние

```zell
enum OrderState { Created, Paid, Shipped, Delivered, Cancelled }

struct Order {
    State OrderState

    func Pay() void! {
        switch this.State {
            case OrderState.Created: this.State = OrderState.Paid
            default: return error("cannot pay in state " + string(this.State))
        }
    }
}
```

### Внедрение зависимостей

```zell
struct UserService {
    repo  UserRepo      // интерфейс
    mail  Mailer        // интерфейс
    log   log.Logger

    static func New(repo UserRepo, mail Mailer, log log.Logger) UserService {
        return UserService{repo: repo, mail: mail, log: log}
    }
}
```

В тестах подставляются структуры-заглушки; в будущем `zell-lang/web` контейнер
собирает граф по типам при компиляции.

## Чего нет и чем заменяется

| В C#/Java | В Zell |
|---|---|
| `abstract class` с общими полями и абстрактными методами | интерфейс для контракта + встраиваемая структура для общих полей/методов |
| `virtual`/`override` | интерфейс, переданный явно (см. «шаблонный метод») |
| `protected` | приватные (строчные) члены видны всему пространству имён |
| Глубокие иерархии | плоские структуры, композиция из нескольких встраиваний |
| `base.Method()` | `this.Base.Method()` — явный вызов встроенного |
| `is`/`as` с классами | `switch x { case T v: }`, работает для интерфейсов и `any` |
| Перегрузка методов | разные имена или generics; нет перегрузки по типам |
| Перегрузка операторов | только через встроенные интерфейсы `Add`, `Mul`, `MatMul`, `Index`, `Equatable`, `Ordered` (DESIGN.md «Перегрузка операторов») |
| Свойства с геттерами/сеттерами | поля; метод, если нужна логика; `@observable` в GUI генерирует уведомления |
| События (`event`) | `events.Hub<T>` или срез обработчиков |
| Исключения и иерархия исключений | значения-ошибки, типизированные структуры, `errors.As` |

## Пример: `implements`, методы по умолчанию и `sealed`

```zell
namespace app.shapes

import io

sealed interface Shape {
    func Area() float
    func Name() string

    // Метод по умолчанию: выражен через остальные методы интерфейса.
    func Describe() string {
        return fmt.Format("{0} with area {1}", this.Name(), this.Area())
    }
}

// Ошибка «не реализует» показывается здесь, а не в месте использования.
struct Circle implements Shape {
    R float
    func Area() float  { return math.Pi * this.R * this.R }
    func Name() string { return "circle" }
}

struct Rect implements Shape {
    W float
    H float
    func Area() float  { return this.W * this.H }
    func Name() string { return "rect" }
    func Describe() string { return fmt.Format("rect {0}x{1}", this.W, this.H) } // переопределение
}

func Kind(s Shape) string {
    // Shape — sealed: switch должен покрыть все реализации, default не нужен.
    switch s {
        case Circle c: return fmt.Format("round, r={0}", c.R)
        case Rect r:   return fmt.Format("boxy, w={0}", r.W)
    }
}

func main() {
    var shapes []Shape = [Circle{R: 1}, Rect{W: 2, H: 3}]
    for s in shapes {
        println(s.Describe(), "|", Kind(s))
    }
}
```

- `implements` не меняет семантику: интерфейсы структурные. Реализации
  `sealed`-интерфейса — все типы его пространства имён с нужными методами
  (с `implements` или без); тип из другого пространства имён его не
  реализует. Пример лежит в `examples/sealed_shapes.zl`.
- `this` в методе по умолчанию — значение интерфейса; полей у интерфейса нет.
- Новая реализация `sealed`-интерфейса подсвечивает каждый неполный `switch`.

## Расширения языка для паттернов

Три небольших дополнения, которые закрывают оставшиеся неудобства без
наследования:

1. **Методы по умолчанию в интерфейсах.** Интерфейс может содержать метод
   с телом, выраженный через остальные методы интерфейса. Реализация
   обязана дать только абстрактные методы.

   ```zell
   interface Logger {
       func Log(level int, msg string)
       func Info(msg string)  { this.Log(1, msg) }
       func Error(msg string) { this.Log(3, msg) }
   }
   ```

   Это не наследование состояния: у интерфейса нет полей, `this` внутри
   тела — значение интерфейса.

2. **Встраивание интерфейсов.** `interface ReadWriter { Reader; Writer }`
   объединяет методы.

3. **`sealed` интерфейсы.** Реализовать `sealed interface Shape` можно
   только в том же пространстве имён. Тогда компилятор знает все
   реализации, и `switch` по типу проверяется на полноту так же, как для
   enum. Это закрытые иерархии: узлы AST, события, команды.

4. **Опциональная проверка `implements`.** `struct Circle implements Shape`
   ничего не меняет в семантике (интерфейсы остаются структурными), но
   ошибка «не реализует» показывается на объявлении структуры, а не в
   далёком месте использования. Удобно для публичных API.
