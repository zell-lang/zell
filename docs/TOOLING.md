# Инструменты Zell: LSP, редакторы, форматтер, отладчик

Цель: компилятор с первого дня проектируется как библиотека, которой
пользуются все инструменты. Ни один инструмент не парсит `.zl` сам.

## Архитектура компилятора как библиотеки

```
zell (CLI)  zell-lsp  zell fmt  zell test  IDEA plugin (через LSP)
     \          |          |          |              /
      +---------+----------+----------+-------------+
                      |
              crate `zell` (библиотека)
   session -> lexer -> parser -> sema (names, types) -> codegen
```

Требования к библиотеке, из которых исходят все остальные инструменты:

1. **Парсер устойчив к ошибкам** (сделано). `parser::parse_file` на любом
   вводе возвращает `File` с узлами `ExprKind::Error`, `StmtKind::Error`,
   `TypeKind::Error`, `Decl::Error` (`parser::parse` — обёртка, дающая
   `None` при ошибках, для фаз, которым нужен чистый AST). Восстановление:
   пропущенный операнд — `Error`-узел нулевой ширины; `x.` без имени —
   `Field { base, name: "" }` (для дополнения); незакрытый `f(` — `Call` с
   `Error`-аргументом в конце; плохой элемент списка аргументов/литерала
   пропускается до `,` или закрывающей скобки; неудачная инструкция — до `;`
   или `}` своего блока (`StmtKind::Error`); ключевое слово объявления
   (`func Name`, `struct`, ...) внутри тела значит, что не хватает `}`, и
   тело закрывается там; `}` в конце файла закрывает все блоки; неудачное
   объявление пропускается до следующего ключевого слова объявления
   (`Decl::Error`). Sema даёт `Error`-узлам тип `Invalid` без диагностик и
   пропускает проверки всего тела (неиспользуемые переменные, missing
   return), если в теле есть ошибки разбора. Тест: каждый префикс каждого
   файла `examples/`, `tests/spec/`, `benchmarks/` даёт `File` без паники.
2. **Все узлы несут `Span`**, а лексер сохраняет комментарии и переносы
   строк как trivia (сделано): `lexer::lex_with_trivia` прикрепляет к
   каждому токену *ведущие* trivia (`LineComment`, `BlockComment`,
   `Newlines(n)`; автоматические `;` нулевой ширины их не получают). AST
   trivia не хранит: форматтер заново лексирует файл и обходит AST по
   span'ам — этого достаточно, чтобы сохранять комментарии и одиночные
   пустые строки.
3. **Инкрементальность по пространствам имён** (сделано, см. «LSP →
   Производительность»). `Session` хранит разобранные файлы (с `@cfg`) и
   кэш результатов *единиц анализа* — файлов одного пространства имён.
   Изменение файла пересчитывает его единицу и единицы, которым видны
   изменившиеся объявления; API тот же: `Session::update_file(path, text)`
   и `Session::analyze()` (`analyze_cancellable` для отмены,
   `analyze_full` — эталонный анализ без кэша). Благодаря устойчивому
   парсеру каждый файл анализируется по *текущему* тексту (без «последнего
   удачного AST»), так что позиции всегда актуальны; `Analysis` отдаёт и
   AST файлов (`asts`) для подсказок LSP.
4. **Семантическая модель доступна снаружи.** После `sema` для каждого
   узла известны: разрешённый символ (`SymbolId`), тип (`TypeId`),
   определение (файл + span), документация (комментарий перед
   объявлением). Это таблицы, а не только побочный эффект кодогенерации.
5. **Диагностики структурированы** (сделано): `Diagnostic` несёт span,
   severity, заметки, необязательный код (`diagnostics::codes`) и список
   **quick fix** (`Fix { title, edits: Vec<TextEdit> }`). Коды и правки:
   `E0001` неиспользуемая переменная → переименовать в `_` / удалить
   объявление; `E0002` результат `T!` не обработан → дописать `?` (только
   где `?` допустим: функция возвращает `T!` или внутри блочного `try`) /
   обернуть в `try ... catch err { panic(err.Message()) }`; `E0003`
   неиспользуемый импорт → удалить строку; `E0004` неизвестный
   идентификатор, который есть в stdlib → `import xxx`; `E0005` импорт
   удалённого корня `std` (`import std.io`) → `import io`; `E0100`
   зарезервирован для синтаксических ошибок; `W0001` (предупреждение)
   тип может образовать цикл ссылок (DESIGN.md «Циклы»; `@allowCycle`
   глушит, `--deny-warnings` делает все предупреждения ошибками). Sema заполняет правки в местах
   выдачи диагностик; `Session` дополняет то, что требует текста и AST
   (удаление целой строки, удаление `var`).
6. **Стабильные позиции**: внутри компилятора — байтовые смещения,
   на границе LSP — конвертация в строку/колонку в согласованной кодировке
   (UTF-8/16/32, `SourceFile::position_in`/`offset_in`).

## LSP-сервер (`zell lsp`)

Отдельный бинарник или подкоманда, крейт `tower-lsp` (или `lsp-server`
из rust-analyzer). Транспорт stdio. Функции по приоритету:

| Этап | Возможности LSP | Что нужно от компилятора |
|------|-----------------|--------------------------|
| 1 | diagnostics, document symbols, folding | устойчивый парсер, spans |
| 2 | go to definition, hover (тип + документация), find references, rename | таблицы символов и типов, индекс ссылок |
| 3 | completion (члены после `.`, имена в области видимости, импорты, ключевые слова), signature help | семантика по позиции курсора, частичный AST |
| 4 | semantic tokens (точная подсветка), inlay hints (выведенные типы `var x = ...`, имена параметров), code actions / quick fixes | типы выражений, структурированные диагностики |
| 5 | formatting (через `zell fmt`), workspace symbols, call hierarchy | trivia в AST, индекс проекта |

Состояние (`lsp/`): этапы 1–2 целиком; этап 3 — completion и
`textDocument/signatureHelp` (триггеры `(` и `,`, активный параметр,
сигнатура с именами и типами, документация); этап 4 — semantic tokens,
`textDocument/inlayHint` (выведенный тип после имени в `var x = ...` и
`for x in ...`, имена параметров перед литеральными аргументами;
настройки `zell.inlayHints.types.enabled` и
`zell.inlayHints.parameterNames.enabled` через `initializationOptions` и
`workspace/didChangeConfiguration`), `textDocument/codeAction` (quick fix
из структурированных диагностик), `textDocument/documentHighlight`; этап 5
— `textDocument/formatting`, `textDocument/rangeFormatting` (только
изменённые строки внутри выделения), `workspace/symbol` (нечёткий поиск по
имени). `workspace/didChangeWatchedFiles` подхватывает созданные,
изменённые и удалённые на диске `.zl` (открытые в редакторе файлы
главнее). Исходники `std/` и зависимостей из `.zell/deps/` (или
`vendor/`) по `zell.lock` индексируются как файлы проекта только для
чтения (см. ниже). Не сделано: call hierarchy, linked editing,
инкрементальный анализ (каждый анализ — весь проект, разбор кэшируется
по файлам).

Методы сервера (`lsp/src/`; capability — что объявляется в `initialize`):

| Метод | Capability | Где |
|-------|-----------|-----|
| `textDocument/didOpen`, `didChange`, `didSave`, `didClose` | `textDocumentSync`: `change: Incremental` (2), `save.includeText` | `lib.rs`, `sync.rs` |
| `initialize`, `shutdown`, `exit` | `positionEncoding`: первая из `utf-8`/`utf-16`/`utf-32` в `general.positionEncodings` клиента, иначе `utf-16` | `lib.rs`, `encoding.rs` |
| `$/cancelRequest` | — | `lib.rs`, `snapshot.rs` |
| `textDocument/publishDiagnostics` (push) | — | `lib.rs` |
| `textDocument/diagnostic`, `workspace/diagnostic` (pull) | `diagnosticProvider { identifier: "zell", interFileDependencies, workspaceDiagnostics }` | `handlers.rs` |
| `textDocument/definition`, `declaration` (= definition) | `definitionProvider`, `declarationProvider` | `handlers.rs` |
| `textDocument/typeDefinition` | `typeDefinitionProvider` | `nav.rs` |
| `textDocument/implementation` | `implementationProvider` | `nav.rs` |
| `textDocument/references`, `documentHighlight` | `referencesProvider`, `documentHighlightProvider` | `handlers.rs`, `assist.rs` |
| `textDocument/hover` | `hoverProvider` | `features.rs` |
| `textDocument/documentSymbol`, `workspace/symbol` | `documentSymbolProvider`, `workspaceSymbolProvider` | `features.rs`, `assist.rs` |
| `textDocument/prepareRename`, `rename` | `renameProvider { prepareProvider }` | `handlers.rs` |
| `textDocument/completion`, `completionItem/resolve` | `completionProvider { triggerCharacters: ["."], resolveProvider }` | `completion.rs` |
| `textDocument/signatureHelp` | `signatureHelpProvider` (`(`, `,`) | `assist.rs` |
| `textDocument/inlayHint`, `inlayHint/resolve` | `inlayHintProvider { resolveProvider }` | `assist.rs` |
| `textDocument/codeAction`, `codeAction/resolve` | `codeActionProvider { codeActionKinds: [quickfix, refactor.extract, refactor.inline, source.organizeImports], resolveProvider }` | `assist.rs`, `refactor.rs` |
| `textDocument/codeLens`, `codeLens/resolve` | `codeLensProvider { resolveProvider }` | `lens.rs` |
| `textDocument/semanticTokens/full`, `full/delta`, `range` | `semanticTokensProvider { full: { delta: true }, range: true }` | `features.rs` |
| `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `outgoingCalls` | `callHierarchyProvider` | `hierarchy.rs` |
| `textDocument/foldingRange`, `selectionRange` | `foldingRangeProvider`, `selectionRangeProvider` | `features.rs`, `nav.rs` |
| `textDocument/formatting`, `rangeFormatting` | `documentFormattingProvider`, `documentRangeFormattingProvider` | `assist.rs` |
| `textDocument/onTypeFormatting` | `documentOnTypeFormattingProvider` (`}`, `\n`) | `assist.rs` |
| `textDocument/documentLink` | `documentLinkProvider` | `links.rs` |
| `workspace/didChangeWatchedFiles`, `didChangeConfiguration` | наблюдатели регистрируются сервером (`client/registerCapability`), если клиент объявил `didChangeWatchedFiles.dynamicRegistration` | `lib.rs`, `workspace.rs` |
| `workspace/configuration` (сервер → клиент) | если клиент объявил `workspace.configuration` | `workspace.rs` |
| `workspace/didChangeWorkspaceFolders` | `workspace.workspaceFolders { supported, changeNotifications }` | `workspace.rs` |
| `workspace/semanticTokens/refresh`, `inlayHint/refresh`, `codeLens/refresh` (сервер → клиент) | если клиент объявил `refreshSupport` | `workspace.rs` |
| `$/setTrace`, `$/logTrace` | `initialize.trace` | `workspace.rs` |
| `workspace/willRenameFiles`, `didRenameFiles` | `workspace.fileOperations { willRename, didRename }` (`**/*.zl`, папки) | `files.rs`, `lib.rs` |
| `window/workDoneProgress/create` + `$/progress` (сервер → клиент) | если клиент объявил `window.workDoneProgress` | `lib.rs` |
| `zell/analyzerStatus`, `zell/documentText` | служебные: счётчики планировщика, текст документа у сервера (для отладки и тестов) | `handlers.rs` |

Подробности:

- **Жизненный цикл** (LSP 3.17): запросы до `initialize` — ошибка
  `ServerNotInitialized` (−32002); после `shutdown` — `InvalidRequest`, а
  ждущие в очереди отменяются; `exit` без `shutdown` — код выхода 1.
  Паника в обработчике отвечает `InternalError`, сервер продолжает работу.
  Соответствие спецификации по методам — `docs/LSP_CONFORMANCE.md`.
- **Rename**: `WorkspaceEdit` одним ответом на все файлы (в том числе
  неоткрытые); `documentChanges` с версиями открытых документов, если
  клиент объявил `workspace.workspaceEdit.documentChanges`, иначе
  `changes`. Переименование, при котором имя совпало бы с видимым в месте
  использования (локальная, член пространства имён, импорт) или с соседним
  членом, отклоняется с объяснением; `prepareRename` там, где нет символа, —
  `null`, на символе std/зависимости — ошибка с причиной.
- **Синхронизация** инкрементальная: диапазоны `contentChanges` в
  согласованной кодировке применяются к копии текста у сервера по порядку (`\r` перед `\n` не часть
  строки, позиции за концом строки/текста прижимаются к концу); изменение
  без `range` заменяет весь текст. Разбор откладывается до старта анализа,
  поэтому серия `didChange` стоит одного разбора и одного анализа.
- **Планировщик и отмена.** Анализ идёт в отдельном потоке над снимком
  `Session` (клон дешёвый: разобранные файлы общие), через ~100 мс после
  последней правки или сразу, если ждёт запрос. Правка отменяет анализ в
  полёте (`AtomicBool`, sema проверяет его между фазами и перед телом
  каждой функции: `Session::analyze_cancellable`). Запросы отвечают по
  анализу последнего текста; пока его нет, они ждут в очереди, и
  `$/cancelRequest` вынимает их оттуда с ошибкой `RequestCancelled`
  (−32800). Форматирование, folding и selection range нужны только текст
  и синтаксис — отвечают сразу. `initializationOptions.debug.analysisDelayMs`
  искусственно замедляет анализ (для тестов).
- **Параллельные запросы** (`lsp/src/snapshot.rs`). Главный цикл владеет
  изменяемым состоянием (сессия, тексты документов, анализ в полёте) и
  только читает сообщения, запускает анализ и раздаёт работу; запросы,
  которым нужен анализ, выполняются в пуле из 2–4 потоков над снимком
  `Snap`: `Arc<Analysis>` (неизменяемый, `Send + Sync`) и таблицы URI и
  версий документов за `Arc` (copy-on-write в главном цикле), так что
  снимок — несколько счётчиков ссылок, без копий. Разделение:
  - *в пуле*: hover, definition/declaration/typeDefinition/implementation,
    references, documentHighlight, documentSymbol, workspace/symbol,
    prepareRename/rename, semanticTokens (full, delta, range), completion
    и resolve, signatureHelp, inlayHint и resolve, codeAction и resolve,
    codeLens и resolve, documentLink, callHierarchy (prepare, incoming,
    outgoing), textDocument/diagnostic, workspace/diagnostic;
  - *в главном потоке* (последний текст, без анализа или с изменением
    состояния): formatting, rangeFormatting, onTypeFormatting,
    foldingRange, selectionRange, workspace/willRenameFiles,
    `zell/analyzerStatus`, `zell/documentText`, shutdown; все уведомления.

  Запрос, пришедший при устаревшем анализе, ждёт новый (как и раньше), а
  не отвечает по старому снимку: позиции в запросе — в последнем тексте,
  поэтому completion/hover/signatureHelp никогда не считаются по чужому
  смещению; это же правило для запросов без позиции (workspace/symbol,
  resolve), потому что старый анализ освобождается при старте нового.
  В пуле у запроса флаг состояния: `$/cancelRequest` — ответ
  `RequestCancelled` (−32800), правка его документа — `ContentModified`
  (−32801); правка любого файла проекта останавливает запросы с
  результатом на весь проект (references, rename, workspace/symbol,
  incoming/outgoing calls, codeLens/resolve). Поток проверяет флаг перед
  стартом, в длинных циклах (ссылки, символы, отчёт workspace/diagnostic,
  правки rename) и перед ответом, так что на запрос всегда отвечает
  ровно один раз его поток. Запросы по другим документам отвечают по
  своему снимку. Порядок: уведомления, отправленные до раздачи запроса
  (`publishDiagnostics` нового анализа), приходят клиенту раньше ответа;
  ответы разных запросов из пула могут прийти в любом порядке.
  Кодировка позиций (thread-local) передаётся в снимке.
  `initializationOptions.debug.requestDelayMs` (`{"метод": мс}`)
  замедляет обработчики для тестов; `zell/analyzerStatus` показывает
  `running` — запросы в пуле.
- **Диагностики**: push или pull, не оба: `initializationOptions.diagnostics.mode`
  = `auto` (по умолчанию: pull, если клиент объявил
  `textDocument.diagnostic`, иначе push), `push` (pull не объявляется) или
  `pull`. `resultId` — хэш диагностик файла (`unchanged`, если совпал);
  `workspace/diagnostic`, в котором всё `unchanged`, «паркуется» до
  анализа, который что-то меняет (клиенты, переспрашивающие сразу, не
  крутятся); после анализа — `workspace/diagnostic/refresh`, если клиент
  его поддерживает. С `partialResultToken` отчёт `workspace/diagnostic`
  идёт по файлу в `$/progress`, ответ — пустой. В push у
  `publishDiagnostics` (и в отчётах `workspace/diagnostic`) — `version`
  открытого документа. У диагностик с quick fix'ами в `data` лежат сами
  исправления (`{"v": версия документа, "fixes": [{title, edits}]}`):
  `codeAction` берёт их из `context.diagnostics`, если версия совпадает,
  иначе пересчитывает по анализу. Заметки о другом файле (`related`
  диагностики, например «previously declared here» у дубликата из другого
  файла пространства имён) — `relatedInformation` с URI того файла.
- **Состояние workspace** (`workspace.rs`). После `initialized` сервер
  регистрирует наблюдатели `**/*.zl`, `**/zell.toml`, `**/zell.lock`
  (если клиент умеет dynamic registration; VS Code-клиент свой наблюдатель
  больше не создаёт) и запрашивает секцию `zell` через
  `workspace/configuration` — и снова на каждый `didChangeConfiguration`;
  `initializationOptions` и `settings.zell` в `didChangeConfiguration`
  остаются запасным путём. Изменение `zell.lock`/манифеста корня
  перезагружает исходники зависимостей. Папки, добавленные
  `didChangeWorkspaceFolders`, загружаются как корни, удалённые —
  выгружаются (открытые документы остаются). Устаревший `didChange`
  (версия ниже записанной) отбрасывается. `$/setTrace` (`messages`,
  `verbose`) включает `$/logTrace`: время каждого запроса и анализа,
  отброшенные правки.
- **Refresh.** После анализа сервер сравнивает отпечатки индекса:
  объявления (вид, полное имя, сигнатура) и число ссылок на каждое. Если
  изменились объявления и открыт документ кроме правленых —
  `workspace/semanticTokens/refresh` и `workspace/inlayHint/refresh`; если
  изменились объявления или число ссылок — `workspace/codeLens/refresh`
  (каждый — не больше одного на анализ и только при `refreshSupport`).
  Смена настроек inlay hints — `workspace/inlayHint/refresh`.
- **On-type formatting**: на `}` — блок, который он закрыл, на перевод
  строки — самый внутренний блок вокруг курсора; формат `zell::fmt` всего
  файла, но в ответе только правки целиком внутри блока (после перевода
  строки — не трогая строку курсора). При синтаксической ошибке и на
  верхнем уровне — ничего. VS Code-клиент включает `editor.formatOnType`
  для `[zell]` по умолчанию.
- **Document links**: путь каждого `import a.b` (и `import a.b.T`) — ссылка
  на файл, где пространство имён объявлено первым (символ namespace; для
  std — первый исходник с его членом). Пространство имён может занимать
  много файлов и папок, поэтому ссылка ведёт на файл, а не на папку
  (папку редакторы не открывают). Пути в `zell.toml` ссылками не
  становятся.
- **Semantic tokens**: на документ хранится последний результат с
  `resultId`; `full/delta` от него — одна правка между общим префиксом и
  суффиксом (в единицах массива: 5 чисел на токен), от неизвестного id —
  полные токены; `range` — токены, пересекающие диапазон.
- **Implementation**: интерфейс → структуры и enum'ы, у которых есть все
  его методы с теми же сигнатурами (методы с телом по умолчанию можно не
  иметь; методы встроенных структур считаются); sealed — только из его
  пространства имён; метод интерфейса → реализующие методы. **Type
  definition**: тип переменной/параметра/поля/константы, результат функции
  (сквозь `*T`, `weak`, `[]T`, `[N]T`, `T!`, `Task<T>`, значения `map`); для
  экземпляра обобщённого типа — его объявление-шаблон.
- **Члены после `.`**: тип получателя берётся из sema, а не из текста.
  Парсер восстанавливает `x.` как `Field` с пустым именем, sema
  проверяет `x` и для каждого обращения к члену (`x.name`, `x.name(...)`,
  `x.`) записывает тип `x` по началу имени (`SymbolIndex::member_recvs`,
  через фрагменты инкрементального анализа). Поэтому получателем может быть
  любое выражение: вызов (`f().`, `x.Get().Name.`, fluent-цепочки),
  индекс (`xs[0].`, `m["k"].`), `(await g()).`, скобки, тело лямбды;
  `await x.M().` — член `Task<T>` (`await` относится ко всей цепочке).
  Список членов — по типу: поля, нестатические и продвинутые методы
  пользовательских структур (для экземпляра обобщённой — поля с
  подставленными типами в `detail`), члены std-структур (включая
  `@intrinsic`), `Task<T>`, методы интерфейса, `error` (`Message`),
  методы enum, элементы кортежа `0`, `1`, … Видимость — как у sema
  (свои члены — в своём пространстве имён, чужие — экспортированные).
  Если записи нет (пространство имён `encoding.json.`, тип `Point.`), —
  прежний разбор цепочки имён из текста. Получатель `T!` (до `?`) даёт
  члены `T` без правки (`?` допустим только в функции `T!`, поэтому
  `?`/`try` дописывает пользователь); `weak *T` — члены `T`; параметр
  типа с ограничением-интерфейсом (`func F<T: Shape>`) — методы
  интерфейса (их записывает абстрактная проверка шаблона,
  `GenState::abstract_recvs`); без ограничения — ничего.
- **Качество completion** (`lsp/src/completion.rs`): у каждого элемента
  `textEdit` с точным диапазоном — префикс слова под курсором (при
  `insertReplaceSupport` — `InsertReplaceEdit`: insert = префикс, replace
  = всё слово). Список фильтруется по префиксу (подпоследовательность без
  учёта регистра), сортируется сервером и несёт `sortText`: сначала
  совпадения по префиксу, затем локальные > параметры > поля > прочие
  члены > имена своего пространства > импортированные/std/встроенные >
  авто-импорт > ключевые слова; внутри — члены в порядке объявления
  (поля раньше методов), локальные — ближайшее объявление первым,
  остальное по имени. Больше 200 элементов — обрезка после сортировки и
  `isIncomplete: true`. Только при `snippetSupport`: вызовы
  `name(${1:a}, ${2:b})` / `name()$0` (не если `(` уже стоит после слова)
  и сниппеты операторов в начале инструкции (`if`, `for`, `switch`,
  `defer`, `defer await` — только в async-функции, `try`, `return` — с
  `$0`, если функция что-то возвращает); иначе — простой текст.
  Ключевые слова по контексту: вне функции в начале строки — только
  слова объявлений; в середине выражения — без операторных
  (`for`, `return`, `var`, …). Авто-импорт: при непустом префиксе
  экспортированные типы, функции, константы и глобалы неимпортированных
  пространств имён (std — все), имя которых начинается с префикса и ещё
  не видно, с `labelDetails.description = "import ns"` и
  `additionalTextEdits`, вставляющим `import ns` в порядке organize
  imports (`refactor::import_edit`).
- **Resolve**: элементы completion несут `data` (id символа, полное имя,
  версия анализа); `completionItem/resolve` добавляет `detail` и Markdown-
  документацию (тот же конвейер, что hover). Подсказки inlay — tooltip в
  `inlayHint/resolve`. Quick fix'ы дешёвые и сразу с правкой:
  `codeAction/resolve` возвращает их как есть. Рефакторинги
  (`lsp/src/refactor.rs`) при `codeAction.resolveSupport` с `edit` приходят
  без правки, с `data` (вид, файл, выделение), и правка считается в
  `codeAction/resolve`; иначе сразу с правкой.
- **Рефакторинги** (code actions, фильтр `only` учитывается):
  `refactor.extract` — «Extract variable» (выделенное выражение →
  `var name = ...` перед оператором; имя из вызываемой функции/поля без
  `Get`, уникальное в функции) и «Extract function» (выделенные целиком
  операторы одного блока → новая функция после текущей; параметры —
  внешние локальные переменные/параметры, которые читает диапазон;
  результат — объявленные в диапазоне и используемые после него, один или
  кортежем; `async` + `await` при `await`, `!` + `?` при `?`; диапазоны с
  `return`/`break`/`continue` не предлагаются; в методе — метод и вызов
  через `this.`); `refactor.inline` — «Inline variable» (локальная `var` с
  инициализатором без присваиваний и `&x`: подстановка, в скобках если
  нужно, и удаление объявления); `source.organizeImports` — сортировка
  импортов и удаление неиспользуемых (`E0003`). Quick fix'ы инспекций — см.
  «Инспекции».
- **Code lens**: на `@test`/`@bench`-функциях «▶ Run test» (`zell.runTest`)
  и «Debug test» (`zell.debugTest`) с аргументами `[uri файла, имя]`, на
  `main` — «▶ Run» (`zell.run`, `[uri]`); на экспортируемых объявлениях
  верхнего уровня — «N references», счёт в `codeLens/resolve` (команда
  `zell.showReferences` с `[uri, position, locations]`). Команды
  выполняет клиент (`editors/vscode/src/extension.ts`: `zell test <пакет>
  --filter ^Имя$ -v` и `zell run` как задачи; «Debug test» может
  запускать `zell test --debug --wait --filter ^Имя$` и подключаться к
  агенту, см. «Тесты»).
- **Selection range**: токен → выражения → оператор → блоки → функция →
  объявление → файл, по span'ам AST.
- **Прогресс**: начальная индексация (загрузка std, файлы проекта, первый
  анализ) и подгрузка проекта, найденного по манифесту при открытии файла.
- **Переименование файлов**: правки только для файлов пакета, чьё
  пространство имён выводится из пути (`src/a/b/x.zl` пакета с корнем
  `app` → `namespace app.a.b`), когда путь меняет это имя: строка
  `namespace` перемещаемого файла; если пространство имён переезжает целиком
  — `import app.a.b` и `import app.a.b.T` везде; иначе — `import app.a.b.T`
  членов файла, `import <новое>` в файлах, которые используют его
  объявления, и `import app.a.b` в самом файле, если он использует
  оставшиеся. Полные пути без импорта не переписываются.
  `didRenameFiles` переносит файлы в сессии.

Анализ инкрементальный (см. «Производительность»); диагностики
публикуются только для файлов, где они изменились.

Проект определяется файлом `zell.toml`; LSP ищет его вверх от открытого
файла, загружает все `.zl` проекта и стандартную библиотеку (исходники
`std` поставляются с компилятором, поэтому go-to-definition в `std` работает).

### Производительность (Performance)

Анализ в `Session` инкрементальный (`compiler/src/session/`: модули
`incr`, `fragment`, `assemble`, `facts`, `remap`, `stdblock`; со стороны
sema — режим единиц `sema::units`).

- **Единицы анализа.** Файлы делятся на единицы — файлы одного
  пространства имён (все файлы пространств стандартной библиотеки — одна единица; так
  бывает, когда открыт каталог `std/`). Единица зависит от единиц, которые
  может назвать: потомков своего пространства, импортов (и их префиксов),
  пространств под корнем, имя которого встречается в ней как
  идентификатор (корни видны без импорта); единицы, использующие
  `data`, зависят друг от друга (модель строится по всем контекстам
  программы); зависимости транзитивны.
- **Ключ кэша** единицы — хэши текстов её файлов и *экспортных сигнатур*
  файлов её зависимостей (`session/facts.rs`): токены вне тел
  негенерических функций (тела `Configure` читает `data` — они
  входят), позиции токенов обобщённого кода (его диагностики сообщаются в
  этих позициях) и форма объявлений после восстановления парсера (имена,
  флаги, типы). Правка тела меняет ключ только своей единицы, правка
  объявления — ещё и ключи тех, кому оно видно. Если у единицы есть
  диагностики в файлах других единиц (инстанции их шаблонов, модель
  `data`), изменение текста этих файлов тоже делает её грязной.
- **Пересчёт.** Все грязные единицы анализируются одним прогоном вместе
  с объединением их зависимостей, которые идут как файлы *только с
  объявлениями*: их объявления, глобалы и шаблоны проверяются, тела
  функций — нет; на первом анализе (кэш пуст) прогон идёт по всем файлам.
  Правка тела — прогон своей единицы и её зависимостей; правка объявления
  — её единицы и обратных зависимостей (единиц, в замыкании которых она
  есть), остальные берутся из кэша. Файлы только с объявлениями не входят
  в индекс прогона: ссылки и приёмники в них не записываются, а их
  символы создаются лениво — только те, на которые ссылаются полные
  файлы, и только с видом и полным именем (без сигнатур и документации).
  Результат прогона режется на
  фрагменты единиц без id прогона: символы своих файлов, ссылки на чужие —
  по стабильным ключам (вид + полное имя), типы — в мини-программе с
  объявлениями по полному имени. Каждая диагностика относится к файлам,
  чей анализ её выдаёт: к своему файлу, к файлу проверяемого тела или, для
  общего кода из кэша (инстанции обобщений, модель `data`), ко всем
  запросившим. Проверки по всей программе делаются при сборке `Analysis`
  (дёшево, без анализа): объявления пространств имён (определяет первый
  файл), линты и неиспользуемые импорты (только в программе без других
  ошибок — как в прогоне по всей программе: прогон единицы не видит ошибок
  других единиц), сигнатура `main` точки входа.
  `reflect.TypesWith<A>()` в анализе для IDE ничего не сообщает о
  программе в целом (тип всегда `[]*TypeInfo`), поэтому прогону единицы
  неполный список не мешает. Освобождение памяти прогона и прошлого
  `Analysis` (с `malloc_trim`) идёт в отдельном потоке. Тест
  `compiler/tests/incremental.rs` сравнивает инкрементальный анализ с
  полным (`Session::analyze_full`) после каждой правки случайной
  последовательности над `examples/`, `tests/spec/` и сгенерированным
  проектом (`INCR_EDITS`, `INCR_SEED`), а также после сценария правок
  (`scripted_edits_equal_full_analysis`: тело, сигнатура с ошибкой в
  зависимом пространстве, переименование, добавление и удаление файлов и
  импортов, обобщение из другого пространства, атрибут `TypesWith` в
  другом пространстве, `main`): диагностики, символы с типами, ссылки,
  факты файлов и члены пространств имён совпадают.
- **Stdlib.** Индекс std (пространства и члены, места объявлений и
  документация из `StdIndex`) строится один раз на процесс
  (`session/stdblock.rs`). Модули stdlib на Zell разбираются целиком
  лениво, при первой инстанциации их шаблона: для регистрации хватает
  сводки (имена, виды, параметры типов, интерфейсы; `sema/stdsrc.rs`, так
  же и в компиляторе). В режиме единиц не проверяются тела функций stdlib,
  которые не инстанции, и инстанции шаблонов stdlib только со
  stdlib-типами: они ничего не сообщают о коде пользователя.
- **Память.** Тексты и AST общие у сессии, её клонов и `Analysis` (`Arc`),
  std-файлы — общие с `StdIndex`. Типы фрагментов сливаются в одну
  программу (`Analysis::hir`: типы и объявления типов, без функций),
  которая растёт только новыми объявлениями и перестраивается, когда в ней
  много устаревшего. LSP освобождает прошлый `Analysis` до нового анализа;
  после прогона память возвращается системе (`malloc_trim` на glibc).
  `Analysis::program()` анализирует полную программу (с телами) по
  требованию.

Замеры (release `zell-lsp`, `didOpen` одного файла, затем на каждую
правку `didChange` + `completion` + `semanticTokens/full`; RSS после
правок; драйвер — LSP-клиент на Python, его время входит в мс/правку):

| Рабочая область | До: RSS | До: мс/правку | После: RSS | После: мс/правку |
|---|---|---|---|---|
| 1 файл (`examples/hello.zl`) | 91–94 MB | 62 | 55 MB | 8 |
| `examples/` (71 файл, 5,4 тыс. строк) | 147–156 MB | 210–228 | 84 MB | 17 |
| `tests/spec` (884 файла, 33 тыс. строк) | 284–307 MB | 790–830 | 142 MB (пик 164) | 55 |
| `std/` как проект (187 файлов, 75 тыс. строк) | 224–336 MB | 880–1030 | 190 MB (пик 219) | 500 |

После слияния с планировщиком LSP (анализ в рабочем потоке, отмена,
инкрементальная синхронизация) те же замеры: 1 файл — 57 MB, 11 мс;
`examples/` — 87 MB, 31 мс; `tests/spec` — 165 MB (пик), 61 мс; `std/` —
189 MB (пик), 5 мс для файла, не инстанцирующего шаблоны. Каталог с
2 668 файлами (378 тыс. строк, несколько копий корпусов в одном корне,
то есть одно гигантское пространство имён) — 685 MB и 2,8 с на правку:
единица анализа — пространство имён, и такой проект упирается в её
размер.

Большой проект (2026-10-29): сгенерированный `zell::session::synthetic_file`
— 50 пространств имён × 20 файлов, 232 тыс. строк, цепочка импортов
(каждое пространство импортирует предыдущее и базовое с обобщённой
функцией и атрибутом для `TypesWith`). Замеры — `cargo test --release
--test incremental_bench -- --ignored --nocapture` (`Session::analyze`) и
`cargo test --release --test server bench -- --ignored --nocapture` в
`lsp/` (от `didChange` до ответа запроса, ждущего анализа); правки — в
файле среднего пространства имён (25 зависимых), «верхнее» — в последнем
(49 зависимостей).

| Правка | До: `analyze` | После: `analyze` | До: LSP | После: LSP |
|---|---|---|---|---|
| Первый анализ | 620 мс | 515 мс | 914 мс (с `initialize`) | 813 мс |
| Тело функции | 120 мс | 64–70 мс | 158 мс | 68–74 мс |
| Тело функции, верхнее пространство | 205 мс | 89–98 мс | — | — |
| Сигнатура метода (25 зависимых) | 580 мс | 267 мс | 618–661 мс | 273 мс |
| Новый файл (25 зависимых) | 534 мс | 265–274 мс | — | — |

Что дало выигрыш: один прогон по грязным единицам вместо прогона по всем
файлам, когда их несколько (сигнатура, новый файл); файлы только с
объявлениями без индекса (ссылки не записываются, символы ленивые);
`Symbol::name`/`full_name`/`signature`/`doc` — `Arc<str>` (сборка индекса
копирует символы всех единиц); FxHash для ключей символов, индекс ссылок
по символу — сортировкой подсчётом вместо `HashMap<_, Vec>`; освобождение
памяти в фоне. Осталось (при правке тела в проекте такого размера):
~35 мс — регистрация объявлений зависимостей в прогоне (растёт с
замыканием единицы), ~15 мс — проверка тел своей единицы, ~30 мс —
сборка `Analysis` по всем фрагментам (растёт с размером проекта: символы
и ссылки всех единиц копируются в общий индекс).

`textDocument/implementation` и `typeDefinition` для функций читают
`Analysis::program_partial()` — полную программу с телами, которая
анализируется по требованию один раз на анализ (на `tests/spec` около
0,6 с при первом запросе после правки).

В `std/` правка любого файла std пересчитывает всю единицу стандартной библиотеки
(171 файл); зато эти файлы теперь анализируются в собственных
пространствах имён (11 868 ложных ошибок до, 892 после).


## Исходники стандартной библиотеки и зависимостей для IDE

Как `$GOROOT/src` у Go и `rust-src` у Rust: IDE видит исходники всего, что
компилируется.

- **Stdlib — файлы, не строки в бинарнике.** Модули на Zell лежат в
  `std/<namespace path>/*.zl` (`net.http` → `std/net/http/http_client.zl`);
  компилятор загружает их с диска — из `$ZELL_STD`, `<exe>/../std`
  (установленный toolchain: `bin/zell` рядом с `std/`) или `std/`
  репозитория, — встроенные копии только запасной вариант без каталога
  (или без файла в нём). `zell env ZELL_STD` печатает путь
  (`compiler/src/sema/stdfs.rs`).
- **C-функции получают файлы-объявления.** Для каждой строки таблицы
  `FUNCS` в `stdlib.rs` генерируется `extern func Split(s string, sep string) []string`
  с doc-комментарием (из шестого аргумента `f!` или таблицы
  `sema/stdlib_docs.rs`; без документации — имена параметров `a, b, …`) в
  `std/<ns path>/<last>_extern.zl` (`std/strings/strings_extern.zl`).
  Генератор — `compiler/build.rs`: при каждой сборке компилятора копирует
  `sema/stdlib_src/*.zl`, `sema/reflect_src/*.zl` и `collections` в
  `std/` и пишет extern-файлы; меняет только изменившиеся файлы и удаляет
  лишние. `extern func` — объявление без тела, допустимое только в этих
  файлах. IDE показывает сигнатуру, документацию и место объявления;
  переход в реализацию (`runtime/src/std_*.c`) пока не сделан.
- **LSP индексирует std и зависимости** как часть проекта, только для
  чтения: go-to-definition, hover с документацией, references, workspace
  symbols; rename отказывает. Зависимости берутся по `zell.lock` из
  `.zell/deps/` (жёсткие ссылки на хранилище), поэтому путь в IDE
  стабилен и совпадает с тем, что компилируется. ✅ Реализовано:
  `zell::stdindex::StdIndex` разбирает `std/` (включая extern-файлы),
  `Session::set_std_index` добавляет эти файлы в `Analysis::files`, и
  символы std получают место объявления и doc-комментарий оттуда (функции
  рантайма — `SymKey::Std`, шаблоны на Zell вроде `http.Get` — тоже по
  пути); файлы зависимостей — обычные файлы сессии с
  `Session::set_read_only`. Диагностики для них не публикуются, в
  workspace symbols они идут после символов проекта. Поля и методы
  структур и перечислений std (`testing.T.Fatal`) — тоже `SymKey::Std` по
  пути: sema записывает их вызовы так (`ide_method_ref`), а
  `session/stdblock.rs` объявляет их из `std/`, поэтому после `t.` на
  значении std-структуры есть дополнение, hover и переход. Вызовы, которые
  компилятор понижает сам (`t.Must`, `t.MustFail`, `Task.Cancel`),
  объявлены в std заглушками с `@intrinsic` (DESIGN.md «Атрибуты»):
  дополнение на значении `Task<T>` берёт члены `task.Task`.
- **Документация — комментарии перед объявлениями** в тех же `.zl`;
  `zell doc`, hover и реестр читают один источник.
- Отладчик и трассы ошибок показывают пути внутрь `std/` так же, как в
  пользовательский код.

## Редакторы

- **VS Code**: `editors/vscode/` — TextMate-грамматика для подсветки без
  сервера, плюс клиент LSP (`vscode-languageclient`), запускающий
  `zell lsp`. Отладка через DAP (ниже).
- **IntelliJ IDEA**: `grammar/zell.bnf` в формате Grammar-Kit генерирует
  PSI-парсер и лексер; подсветка, структура файла, сворачивание, парные
  скобки делаются на PSI. Семантика (навигация, дополнение, ошибки) — через
  LSP-клиент платформы IntelliJ (LSP API доступен с 2023.2), чтобы не
  дублировать компилятор на Java.
- **Прочие** (Neovim, Helix, Zed, Sublime): TextMate-грамматика
  (Sublime/Zed) или tree-sitter грамматика (планируется, генерируется
  по тому же BNF) плюс LSP.

Единый источник правды для синтаксиса: `grammar/zell.bnf`. При изменении
синтаксиса обновляются BNF, `compiler/src/parser.rs`, TextMate и тесты.
В CI тест проверяет, что все `examples/*.zl` разбираются и парсером
компилятора, и грамматикой из BNF (через сгенерированный парсер).

## AI agents

Агентам (Claude Code, Codex CLI, Gemini CLI, Cursor, Windsurf, Cline,
Aider, Zed, Copilot) Zell отдаётся двумя каналами; всё лежит в `agents/`
(самодостаточный каталог, может стать репозиторием `zell-lang/zell-agents`),
подробности и исследование других языков — `agents/README.md`.

- **LSP** там, где агент сам умеет язык-серверы. Claude Code — через
  плагин `agents/claude-code` (`lspServers` в
  `.claude-plugin/plugin.json`: `zell-lsp`, `extensionToLanguage:
  {".zl": "zell"}`, push-диагностики): после каждой правки агент видит
  диагностики, плюс инструмент `LSP` (определение, ссылки, hover). Плагин
  несёт и MCP-сервер, хук PostToolUse (`zell fmt` правленого файла и
  `zell check` проекта; ошибки возвращаются агенту) и навыки `zell`
  (язык для агента: синтаксис, импорты без `std`, `T!`/`?`/`try`,
  async, память, `@test`, CLI, формат вывода) и `zell-packages`.
  Установка: `claude plugin marketplace add zell-lang/zell` (корневой
  `.claude-plugin/marketplace.json` указывает на `agents/claude-code`),
  `claude plugin install zell@zell`. Агенты внутри VS Code (Cursor,
  Windsurf, Cline, Copilot) получают диагностики от расширения
  `editors/vscode`.
- **MCP** для всех остальных: `zell-mcp` (`agents/mcp`, крейт рабочей
  области, stdio, подмножество MCP вручную на `serde_json`). Работает на
  `Session`/`Analysis` в процессе, как `zell-lsp`, и перечитывает файлы с
  диска перед каждым вызовом. Инструменты: `diagnostics`, `definition`,
  `references`, `hover`, `document_symbols`, `workspace_symbols`,
  `rename` (по умолчанию dry run → unified diff), `format`,
  `run_tests` (`zell test --json`), `build`, `run`, `doc`
  (`zell::doc::describe`), `std_index` (поиск по `StdIndex`). Позиции —
  `path:line:col` с 1, колонки в символах; вместо колонки можно `name`,
  вместо позиции — `symbol` (`User.Hello`, `strings.Split`). Тесты —
  `agents/mcp/tests/server.rs` через stdio.
- **`zell init --agents claude,codex,gemini,cursor,windsurf,cline,aider,zed,copilot`**
  (или `all`; в существующем пакете пишет только файлы агентов,
  `compiler/src/package/agents.rs`): инструкции (оба навыка без
  frontmatter) секцией между маркерами в `AGENTS.md`/`GEMINI.md`/
  `CONVENTIONS.md` или правилами `.cursor/rules/zell.mdc`,
  `.windsurf/rules/zell.md`, `.clinerules/zell.md`; `zell-mcp` в
  `.codex/config.toml`, `.gemini/settings.json`, `.cursor/mcp.json`,
  `.zed/settings.json`, `.vscode/mcp.json`; для Claude Code —
  `.claude/settings.json` с маркетплейсом и включённым плагином.
  Существующие файлы дополняются, а не перезаписываются.

## Инспекции (lints)

Кроме ошибок компиляции sema проверяет программу инспекциями
(`compiler/src/sema/lint.rs`): это проходы по типизированному HIR после
успешного анализа, поэтому они точны (без эвристик по тексту) и работают
одинаково в `zell check`/`build`/`run` и в LSP. Каждая находка — обычная
диагностика с кодом `L00NN`, уровнем и, где правка механическая, исправлением
(quick fix в редакторе, `zell check --fix` в терминале).

| Код | Имя | Уровень | Что находит | Исправление |
|---|---|---|---|---|
| `L0001` | `unreachable` | warning | код после `return`/`break`/`continue`/`panic` | удалить |
| `L0002` | `shadowed-err` | warning | внутренний `err` скрывает внешний (`catch err` во вложенном `catch err`, `var err`) | переименовать в `err2` |
| `L0003` | `empty-catch` | warning | пустой `catch { }` проглатывает ошибку | `?` (в функции `T!`) или `panic(err.Message())` |
| `L0004` | `self-assign` | warning | `x = x`, `a.f = a.f` | удалить |
| `L0005` | `const-condition` | warning | сравнение литералов (`1 == 2`), сравнение значения с самим собой (`x == x`, кроме float) | заменить на `true`/`false` |
| `L0006` | `unused-param` | hint | параметр приватной свободной функции, который тело не читает (функция не используется как значение) | переименовать в `_` |
| `L0007` | `unused-func` | warning | приватная свободная функция без вызовов и ссылок (кроме `main`, `init` и функций с атрибутами; сгенерированные имена тоже пропускаются: `main`, переименованный `zell test` в пакете с `src/main.zl` (`zellUserMain__`), и экземпляры обобщений; метод enum, реализующий метод интерфейса, который enum удовлетворяет, используется через интерфейс) | удалить |
| `L0008` | `defer-in-loop` | info | `defer` в цикле: выполнится при выходе из функции, и все `defer` функции уходят с open-coded пути в кучу | — |
| `L0009` | `drop-capture` | warning | тип с `Drop` кладёт в своё поле замыкание, захватившее `this`: объект держит сам себя (уточнение `W0001`) | — |
| `L0010` | `import-shadowed` | warning | объявление своего пространства имён скрывает имя, которое приносит и импорт (`const ErrNotFound` при `import net`), — на каждом использовании короткого имени; импортированное доступно полным путём | — |

Не инспекции, а ошибки языка: необработанный результат `T!` (`E0002`, с
исправлениями `?` и `try ... catch`) и неиспользуемые переменные, в том
числе ключ/значение `for k, v in m` (`E0001`, исправление — `_`).

Уровни: `error`, `warn`, `info`, `hint`, `off`. Настройка:

- `zell check|build|run --lint=off|warn|error`: выключить все инспекции,
  оставить их уровни (по умолчанию) или сделать все ошибками;
  `--deny-warnings` делает ошибками и инспекции уровня warning.
- `zell.toml`, таблица `[lints]` пакета (код или имя → уровень):
  ```toml
  [lints]
  L0003 = "error"
  unused-param = "off"
  ```
  Действует на файлы самого пакета; зависимости не инспектируются.
- Подавление в коде: комментарий `// zell:allow L0004` (коды или имена
  через пробел/запятую, `all` — все) на строке находки или на строке перед
  ней. Это комментарий, а не синтаксис языка.
- `--fix`: применить первое исправление каждой находки к файлам на диске
  (пересекающиеся правки пропускаются) и сообщить, сколько применено.

В выводе CLI код стоит в метке: `warning[L0001]: unreachable code`. В LSP
у находок `code`, уровень (`Hint`/`Information` для `hint`/`info`) и тег
`Unnecessary` у `L0001`, `L0004`, `L0006`, `L0007`; исправления приходят
как code actions `quickfix`. В LSP уровни из `[lints]` пока не читаются
(используются уровни по умолчанию), `// zell:allow` работает.

## Форматтер (`zell fmt`)

Канонический стиль, без настроек (как `gofmt`): 4 пробела, фигурные
скобки на той же строке, один пробел вокруг бинарных операторов, без
пробелов внутри скобок, выравнивание типов полей структуры в колонку,
сортировка импортов. Реализация (`compiler/src/fmt/`, сделано): AST +
trivia лексера → документ → принтер с алгоритмом Wadler/Prettier для
переносов длинных строк. Используется LSP для `textDocument/formatting`
и в CI (`zell fmt --check`).

`zell fmt [пути] [--check] [--diff] [--stdin]`. Правила:

- отступ 4 пробела, `{` на той же строке, `} else {`, `case` внутри
  `switch` с отступом, тела `case` ещё на уровень глубже;
- один пробел вокруг бинарных операторов и после `,`/`:`; без пробелов
  внутри `()`, `[]`, `T{...}`; `enum E { A, B }` в одну строку, если так
  написано;
- поля структуры выровнены колонками (`Name  string`), как tabwriter в
  gofmt: блок выравнивания прерывается пустой строкой, комментарием на
  отдельной строке, атрибутом на отдельной строке или не-полем; атрибуты
  остаются на своей строке перед полем, если не были записаны в той же
  строке (тогда они — отдельная колонка); комментарии в конце строк полей
  тоже выравниваются;
- импорты: сначала пространства стандартной библиотеки, затем остальные, пустая строка
  между группами, внутри группы по алфавиту; комментарии переезжают вместе
  со своим импортом; ровно одна пустая строка после `namespace` и после
  импортов;
- между инструкциями, членами и объявлениями сохраняется не больше одной
  пустой строки; в начале и в конце блока пустые строки убираются;
- комментарии остаются на месте: перед узлом (на своей строке), в конце
  строки после узла, внутри пустых тел; блочный комментарий внутри
  выражения остаётся в строке; в списке с комментариями элементы всегда по
  одному на строку;
- ширина строки 100: длинные вызовы, параметры и литералы переносятся по
  одному элементу на строку с запятой в конце; последний аргумент-литерал
  «обнимает» вызов (`append(xs, Item{` ... `})`), лямбда в аргументе тоже
  (`go(func() {` ... `})`); длинные бинарные выражения переносятся после
  оператора одного уровня приоритета (перенос только там, где лексер не
  вставит `;`);
- литерал (`T{}`, `map[K]V{}`, `[...]`), записанный с переносом после
  открывающей скобки, остаётся многострочным; короткое тело `{ return x }`,
  записанное в одну строку, остаётся однострочным, пока помещается;
- литералы сохраняют исходное написание (`0xff`, `1_000`, экранирование).

Гарантии (тесты `compiler/tests/fmt.rs`): идемпотентность
`fmt(fmt(x)) == fmt(x)` и сохранение смысла `parse(fmt(x)) == parse(x)`
(без учёта span'ов и порядка импортов) на всех `.zl` из `examples/`,
`benchmarks/`, `tests/spec/`; 10 эталонных случаев в
`compiler/tests/fixtures/fmt/`. Форматтер сам перепарсивает результат и
отказывается выдавать текст с другим AST или потерянным комментарием.
Файлы с синтаксическими ошибками не форматируются.

## Тесты (`zell test`)

Функции `func TestXxx(t *testing.T)` в файлах `*_test.zl`, параллельный
запуск, табличные тесты, `t.Fatal`, бенчмарки `func BenchXxx`. Вывод в
формате, который LSP и IDE умеют показывать (JSON с `--json`), чтобы
тесты запускались кнопкой из редактора.

**Отладка тестов: `zell test --debug [--port N] [--wait] [--filter F]`.**
Обычный запуск даёт каждому тесту свой процесс (и агенту — свой порт), к
нему IDE не подключиться. С `--debug` выбранные тесты (или все) идут **в
одном процессе под одним агентом** (`docs/DEBUGGER.md`, этап 2): сборка с
агентом, драйвер запускается с `--in-process` и
`ZELL_DEBUG=127.0.0.1:N[,wait]` — те же соглашения, что у `zell debug run
--headless [--wait]`: порт (`--port 0` по умолчанию — свободный)
объявляется в stderr строкой `zell-debug: listening on 127.0.0.1:PORT`,
`--wait` держит запуск до `configurationDone`. Каждый тест — отдельная
задача: паника, `t.Fatal`/`t.FailNow` и `t.Skip` завершают только её (в
этом режиме они завершают задачу паникой, поэтому точка останова на
панику их тоже показывает). Тесты идут по одному (`t.Parallel()` не
параллелит), вывод не перехватывается, а печатается по ходу, перед
каждым тестом — `=== RUN`; итог и коды выхода те же. Примеры
(`// Output:`) в этом режиме не запускаются.

В пакете `zell test` строит и зависимости (как `zell build`, включая
dev-зависимости), но `*_test.zl` зависимостей не компилирует: тесты
запускаются только у самого пакета.

Реализовано (`compiler/src/testrun.rs`, `testing`): `@test func
TestX(t *testing.T)` (и `@test func X()`; обе формы могут быть `async`,
раннер ждёт задачу через `testing.Case.RunAsync`), бенчмарки `@bench func BenchX(b
*testing.B)` (или `@test` с `*testing.B`) для `zell test --bench`. Каждый
тест идёт в своём дочернем процессе; тест, в теле которого есть оператор
`t.Parallel()` (находится при обнаружении тестов), выполняется параллельно с
другими такими тестами — до `--parallel N` (по умолчанию 4) процессов —
после последовательных. `testing.Panics(t, fn)` проверяет панику без
повторного запуска кода: `fn` выполняется в отдельном потоке ОС внутри
*области перехвата паники* (`runtime/ABI.md` §8): паника в этом потоке
не завершает процесс — сообщение сохраняется, поток заканчивается
(`longjmp` к началу области), `Panics` возвращает `true`, а код
теста до и после вызова выполняется один раз, в этом же процессе.
Ограничения: `defer` и деструкторы прерванных кадров `fn` **не
выполняются** (то, чем они владели, утекает, взятые ими блокировки
остаются взятыми), паника в другом потоке (задача, запущенная `fn`)
по-прежнему завершает тест. `testing.PanicsWith(t, fn, want)` дополнительно
проверяет, что сообщение содержит `want`; `testing.PanicMessage()` — текст
последней перехваченной паники. Где перехват недоступен (Windows:
`longjmp` через кадры с SEH-раскруткой), и при `ZELL_TEST_PANICS=process`
остаётся прежний путь: бинарник перезапускает текущий тест в дочернем
процессе до этого вызова и там вызывает `fn`; код теста до вызова тогда
выполняется ещё раз в каждом таком процессе (по одному на вызов
`Panics`), вывод дочернего процесса отбрасывается.

## Отладчик

См. `docs/DEBUGGER.md`: DWARF из LLVM, форматтеры lldb/gdb, DAP, затем
собственный `zell debug` с компилируемыми условиями и выражениями,
быстрее Delve.

## Прочие инструменты

- `zell --help` — список команд; `zell <command> --help` — параметры
  одной команды (`zell run --help`, `zell test --help`).
- `zell init` не обязателен. `zell run`/`build`/`check` принимают один
  файл (`zell run main.zl`), обычный каталог с `.zl` (`zell run .`) или
  минимальный проект: каталог с `main.zl` и `zell.toml` рядом
  (`[package]` с `name`, `version` и `entry = "main.zl"`; файлы
  объявляют `namespace <name>`, пока манифест не задаёт `namespace`).
  `zell init` только создаёт такой манифест и раскладку `src/`.
- `zell doc` — документация из комментариев перед объявлениями
  (markdown), HTML и JSON для IDE. ✅ Реализовано (`compiler/src/doc.rs`,
  `doc/{markdown,html,site}.rs`): `zell doc strings.Split` / `time` /
  путь проекта / модуль зависимости — текст в терминале; `--json`;
  `--html out/` — статический сайт (страница на пространство имён, поиск,
  исходники с якорями строк); `--check [--min N]` — покрытие экспортируемых
  имён документацией; `--site out/ [--base /zell/]` — сайт проекта для
  GitHub Pages (README, `docs/guide`, дизайн-документы, справочник std;
  `.github/workflows/docs.yml`). Соглашения: блок `//` над объявлением,
  первое предложение — краткое описание, Markdown, абзац `Deprecated:`;
  документация пространства имён — комментарий над `namespace`. Примеры —
  `func ExampleSplit()` в `*_test.zl` (`ExampleT_Method`, суффикс
  `_second`), с хвостовым `// Output:` их запускает `zell test`
  (`zell test std` — примеры стандартной библиотеки в `std/**/*_test.zl`);
  hover в LSP рисует тот же Markdown и первый пример символа std. API-
  документация пишется по-английски.
  Навигация сайта (`--site`, `doc/nav.rs`): одна боковая панель на каждой
  странице — сворачиваемые группы Guide, Design и Standard library (дерево:
  корневое пространство имён `net` содержит `net.http`, `net.tls`, …);
  группа текущей страницы открыта, ссылка помечена `aria-current="page"`, а
  под ней вложенный список «On this page» из `h2`/`h3` (на экранах от
  1280px — ещё и правая колонка с подсветкой раздела в поле зрения).
  Панель — обычные `<details>` и встроена в каждую страницу, поэтому
  работает без JavaScript; `nav.js` лишь запоминает открытые группы в
  `localStorage` (`zell-nav-<группа>`), делает scroll-spy и переключает
  тему (`zell-theme`; по умолчанию `prefers-color-scheme`). Уже 900px —
  панель за кнопкой «Menu» (checkbox, без скриптов).
- `zell vet` — статические проверки сверх компилятора (неиспользуемые
  результаты, подозрительные сравнения), общий движок с quick fixes LSP.
- `zell build --emit=llvm-ir|asm` — для отладки компилятора.
- **ThreadSanitizer для программ**: `ZELL_SANITIZE=thread` — компилятор
  ставит `sanitize_thread` на каждую функцию программы, прогоняет LLVM-пассы
  `tsan-module,function(tsan)` (после оптимизации и корутин) и линкует с
  `-fsanitize=thread`; рантайм — `make -C runtime CC=clang tsan-lib`
  (`runtime/build/tsan/libzell_rt.a`, `ZELL_RUNTIME=runtime/build/tsan`).
  Так TSan видит и доступы сгенерированного кода (inline RC, CAS мьютекса,
  загрузки состояния задачи — как `__tsan_atomic*`), а не только рантайма:
  без инструментирования программы его отчёты о гонках с кодом Zell —
  артефакты. `scripts/tsan-run.sh <file.zl>...` делает всё сразу: собирает,
  запускает (`SCALE=small` передаётся программам), пишет логи в
  `build/tsan-run/` и печатает отчёты без повторов (по двум верхним кадрам
  обоих доступов); код возврата 1 при любой гонке. Подавления —
  `runtime/tests/tsan.supp` (каждое с объяснением; сейчас пусто).
- Менеджер зависимостей: `zell add org/pkg@1.2.0`, lock-файл, реестр.

## План реализации инструментов

1. Устойчивый парсер с узлами `Error` и trivia (расширение текущего парсера). — сделано.
2. `Session` API в крейте `zell` и структурированные диагностики. — сделано.
3. `zell lsp` этап 1-2 (на `lsp-server`); клиент для VS Code. — сделано.
4. `zell fmt`. — сделано.
5. Semantic tokens, inlay hints, completion, signature help, code actions. — сделано.
6. DWARF в кодогенерации, DAP-адаптер.
7. IDEA-плагин: Grammar-Kit PSI + LSP-клиент.

Fuzzing рантайма: `make -C runtime fuzz-json` (libFuzzer + ASan/UBSan для `encoding.json`); soak-тест HTTP-сервера `scripts/soak-http.sh` ушёл вместе с `web` (тег `web-in-tree`); см. раздел "Soak and fuzz" в `benchmarks/README.md`.
