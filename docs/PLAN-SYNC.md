# PLAN-SYNC — дешёвая многопоточность и синхронизация без busy-spin

Цель: примитивы синхронизации и переключение потоков с латентностью, недостижимой
на полноценных ОС, **без сжигания ядер busy-spin'ом**. Финал — немодифицированные
Linux/C/C++-приложения (pthread mutex/cond, std::mutex/condition_variable, C11
threads, sem) автоматически получают новую реализацию на Talaria.

## 0. РЕАЛИЗОВАНО (v0.51, SPSC v3 — v0.52) — измеренные результаты

Все 6 этапов реализованы (TODO §10, регресс test.sh 71 тест). Стенд: i7-3820
(Sandy Bridge-E, без WAITPKG/MONITORX) — рабочий ярус на этом железе = mwait в
ядре (KVM: `-overcommit cpu-pm=on`); код ярусов ring-3 (umwait/mwaitx) написан и
активируется на новом железе автоматически (детект CPUID при бооте и в libc).

| метрика | busy-spin / Linux-базлайн | Talaria (сон, 0% CPU в ожидании) |
|---|---|---|
| кросс-ядерное пробуждение записью (ring 0) | IPI-путь 15.8 мкс | **mwait p50 237 нс / p99 338 нс** |
| futex ping-pong RTT кросс-ядерно (немодиф. musl) | Linux futex — единицы-десятки мкс | **p50 165 нс** (было на Talaria: 2 тика, до ~100 мс) |
| tsync wait/wake RTT кросс-ядерно | — | **p50 223 нс** |
| SPSC push→pop (пустая очередь → полный цикл сна) | спин-ring 52–64 нс | **533 нс p50, p99 582–852 нс** |
| SPSC throughput (burst, 2 ядра) | спин 315–375 Mops | **140–168 Mops** (busy-режим той же очереди 203–226; v1 была 9) |
| uncontended mutex lock+unlock | pthread/musl 58–68 тактов | **tsync 45–58 тактов** |
| свитч green thread (один core) | корутина ≈ 10-25 нс | **11 нс** (это и есть корутинный свитч) |
| condvar handoff RTT (один core, файберы) | — | **30 нс** (wait+signal_switch, 2 свитча) |
| SPSC mutex+cv на одном ядре | — | **41 нс/элемент** |
| futex same-core RTT (directed switch, 2 нити BSP) | 2 тика (16-96 мс) | **1.3 мкс** (v0.53b; было 7.7-15.7 — EOI/membarrier в горячем пути) |
| **меж-ВМ** пробуждение через ivshmem (этап 5) | virtio-kick ~10 мкс VMEXIT | **RTT p50 496 нс** (обе ВМ спят в mwait) |

Вердикт по целям: кросс-ядерный SPSC/futex на слове — паритет по порядку величины
со спином (сотни нс против ~100–200 нс) при нуле сожжённых циклов; одноядерный
случай — полный паритет с корутинами под стандартным API mutex/condvar. Авто-
подхват работает тремя слоями: (1) ядерный futex (mwait на user-слове) — ВСЕ
бинари без изменений (включая static и glibc); (2) патченный ld-musl на образе —
динамические бинари через PT_INTERP; (3) libtsync/файберы — нативный API.
Хвосты p99 кросс-ядерных путей: было ~19 мкс (страховочные IPI + потерянные
пробуждения вытесненных kwait-спящих до бэкстопа 64 тиков) — после v0.53b
p99 futex cross-core 0.2-0.3 мкс, musl condvar cross-core p50 1.3 мкс
(разбор пяти корней — POC-RESULTS v0.53b).

**MPSC (v0.53, под модель акторов)** — `tsync_mpsc`: Vyukov bounded (продюсер
fetch_add'ом берёт позицию, публикует слот store'ом per-slot seq), у актора
много отправителей. Консюмер спит kind1-upark'ом НА SEQ-СЛОВЕ своего слота —
публикация слота будит kernel-mwait без сискола с обеих сторон; state==2
(припаркован/hlt) — futex-wake шлёт только продюсер позиции c_waitpos (гонка
закрыта ядерным membarrier'ом, как в SPSC v3); пустая очередь — адаптивный
преспин poll-then-sleep (без него под потоком каждый элемент = mfence+флаг у
конса и wake-СИСКОЛ у продюсера: пропускная падала с 18M до 1.7M элем/с).
Полная — futex на слоте продюсера с 1мс-страховкой. v0.53b: протокол продюсера
переведён на fenced (mfence ~20 тактов перед чтением c_state, upark kind|4) —
без ядерного membarrier на каждую парковку консюмера. Применение: tsync-диспетчер
SObjectizer в актор-бенче (services/actors, TODO §11): RTT актор-актор
кросс-ядерно p50 0.67 мкс на Talaria (musl mutex+condvar после фиксов v0.53b —
2.1 мкс, лучше Linux-гостя в 6 раз; см. POC-RESULTS v0.53b).

**SPSC v3 (доработка по итогам профилирования)** — что дало рост 9 → 140–168 Mops:
1. **upark-сискол с предикатом (kind1/kind2)**: ядро ждёт «пока индекс < target»,
   пере-вооружая монитор САМО — гистерезис полной очереди без сискола на каждое
   изменение слова (сисколы парковки: единицы тысяч на 10M операций).
2. **State-обратная связь от ядра** (слово c_state/p_state): 1 = спит mwait'ом на
   слове (store будящего уже будит — wake-сискол НЕ нужен), 2 = припаркован
   (нужен futex-wake). Fast path будящего: 1 relaxed-load + predicted branch,
   ноль fence — StoreLoad-гонку закрывает асимметричный membarrier ЯДРА
   (IPI-ack всем online-ядрам + перепроверка) на медленном пути паркующегося.
3. **Двери (doorbell-линии) с обеих сторон**: спящий паркуется на отдельной
   линии, а не на индексе — прогресс партнёра не пингует линию с монитором;
   партнёр «звонит» ОДИН раз при достижении target (привязка звонка к target,
   а не к state — дедлок-фри при окне state 1→0→1). Анти-дедлок «финальный
   store индекса не был виден»: kernel перепроверяет предикат пары {&idx, target}
   после membarrier перед сном (kind2).
4. **Раскладка полей по владению**: пишущий владеет линией (tail+c_state+p_target
   у прода; head+p_state+c_target у конса; двери и кеши — отдельные линии) —
   ни одного чужого store в горячую линию партнёра.
5. **Fast path — static inline в tsync.h** (стиль DPDK): граница вызова в горячем
   цикле стоила ~4x пропускной (регистры/store-forwarding); в tsync.c остались
   только slow-пути (push_wait/pop_wait/wake_*_slow).
6. **Адаптивный преспин (poll-then-sleep)** в upark: под давлением данных партнёр
   догоняет за микросекунды — без сискола и без «пилы» (спит один — работает
   другой); при простое — быстрый уход в сон.
Цена сна в латентности: конс-дверь добавляет ~80 нс к пробуждению (533 нс против
450 при сне прямо на tail) — зато tail никогда не мониторится и бёрст не
замедляется. Диагностика встроена: счётчики p_slow/p_parks/c_slow/c_parks +
такты в slow-путях (печатаются syncbench'ем).

Целевые метрики (гейты):
- SPSC-очередь на **двух ядрах** через mutex+condvar ≈ спиновый кольцевой буфер на
  Linux (тот же порядок: «push → pop вернулся» ≤ 2–3× от спина, ~150–300 нс), при
  нуле сожжённых циклов в ожидании.
- SPSC на **одном ядре** через mutex+condvar ≈ корутины (свитч ~20–30 нс, без
  XSAVE/CR3/TLB-эффектов).
- 0 сисколов в steady state с обеих сторон; сискол платит только уходящий спать.

## 1. Почему классические примитивы убийственны (полный разбор)

Помимо очевидного (сисколы, переключение контекста, очереди ядра):

1. **Пробуждение через планировщик, а не «в руки»**: `futex_wake` кладёт поток в
   runqueue → решение балансировщика → возможная миграция (холодные кеши) → IPI
   (~1–2 мкс доставка) → выход целевого ядра из C-state (до десятков мкс из
   глубоких). Итого wake → «поток реально исполняется» = 2–10 мкс.
2. **Косвенная цена свитча**: прямая — 1–3 мкс, но перезаполнение L1/L2/TLB после
   чужого потока — ещё десятки мкс размазанной деградации (FlexSC).
3. **Митигации спекуляций**: KPTI/IBRS/MDS-flush — сискол с ~50 нс вырос до
   300–600 нс, свитч дополнительно чистит предсказатели.
4. **XSAVE**: сохранение/восстановление AVX/AVX-512 (до ~2.5 КБ) на каждом свитче.
5. **StoreLoad в fast path**: даже uncontended pthread_mutex — lock-префиксный RMW
   (~20 циклов + сериализация store buffer) на обеих сторонах.
6. **Thundering herd / конвои**: notify_all будит всех через ядро; handoff-мьютексы
   строят конвой на тайм-слайсах.
7. **Таймерная гранулярность и jitter**: усыпание с таймаутом квантуется тиком;
   RCU/softirq прилетают в чужие потоки.

Вывод: дорог не мьютекс — дорог **путь пробуждения через ядро и общий планировщик**.
Дизайн обязан убрать ядро из fast path полностью: и на стороне ожидания, и на
стороне уведомления.

## 2. Ключевая идея: три яруса ожидания + directed switch

Без busy-spin «ждать дёшево» можно, только если железо умеет будить по записи в
память. На x86 это семейство MONITOR/MWAIT — центр дизайна:

- **umonitor/umwait** (WAITPKG, ring 3): поток вооружает монитор на кеш-линию и
  спит в C0.1/C0.2. Запись в линию с другого ядра будит аппаратно: без сискола,
  без IPI, без планировщика. Латентность = когерентность (~40–100 нс) + выход из
  C0.1 (~десятки нс) ≈ 100–300 нс против 2–10 мкс futex. Это НЕ busy-spin: ядро
  спит, отдаёт ресурсы SMT-соседу (на HT-паре umwait *обгоняет* спин, т.к. не
  душит соседний поток) и энергию.
- **mwait в ядре** — то же для яруса глубокой парковки.
- **Directed switch** («отдай CPU вот этому потоку», мимо runqueue и балансировщика)
  — линия Google switchto → FUTEX_SWAP → UMCG: переключение ~200 нс против
  микросекунд через futex. В mainline Linux UMCG так и не влили — у нас своё ядро.

Иерархия ожидания (эскалация по таймауту, деэскалация мгновенная):

```
Ярус 0: bounded-спин 1–2 захода с pause/tpause      — сглаживает наносекундные гонки
Ярус 1: umonitor+umwait на wait-слове               — 0 сисколов с ОБЕИХ сторон
Ярус 2: upark(): ядро mwait'ит на том же слове,
        если ядру больше нечего делать              — 1 сискол у спящего, 0 у будящего
Ярус 3: настоящий блок (ядро занято другими):
        пробуждение через doorbell-страницу или IPI — редкий холодный путь
```

Принцип: **сискол платит только сторона, которая и так уходит спать**; у будящего
в ярусах 0–2 fast path — одна обычная запись в память (`mov`).

## 3. Модель потоков: M:N с vcore

Как Google Fibers / scheduler activations / Shenango:

- Процесс просит у ядра N **vcore** — закреплённых на физядрах kernel-нитей;
  внутри — user-space планировщик и green threads.
- Свитч green thread = callee-saved (rbx, rbp, r12–r15) + rsp + jmp ≈ 20–30 нс.
  **Без XSAVE вообще** (свитч на границе вызова, SysV ABI: xmm caller-saved), без
  CR3, без TLB-эффектов. Это и есть «не хуже корутин» — корутинный свитч, спрятанный
  под API mutex/condvar.
- Ядро даёт **upcall** (активацию) на vcore при блоке green thread в ядре или
  истечении кванта. Для HFT-режима преемпция выключается.
- Для pthread-совместимости M:N не обязателен: слои 1–2 (§10) работают в 1:1.

## 4. Базовый механизм: wait-word и примитивы

Всё сводится к одному механизму — **wait-word** (u32 в user-памяти, аналог
futex-слова) поверх ярусов 0–3.

**wait/notify** (WaitOnAddress-style):

```c
wait(addr, expected):
    if (*addr != expected) return;            // ярус 0
    umonitor(addr);
    if (*addr != expected) return;            // re-check ПОСЛЕ arm — убивает lost wakeup
    umwait(C0.1, deadline);                   // ярус 1
    if (timeout) upark(addr, expected, ...);  // ярусы 2–3

notify(addr):
    *addr = new_val;                          // обычный mov — достаточно для ярусов 0–1
    if (unlikely(sleep_state != AWAKE))       // соседнее поле в той же линии
        doorbell / uwake(tid);                // только ярусы 2–3
```

Гонка arm/check решается порядком «вооружить монитор → перепроверить»: запись
между umonitor и umwait взводит armed-флаг, umwait вернётся немедленно.

**Mutex**: fast path — один CAS (дешевле для MPMC нельзя). Contended —
**MCS-очередь, где каждый ожидающий umwait'ит на своём узле**: unlock = запись в
слово конкретного преемника. Итог: qspinlock без спина, точечное пробуждение (нет
thundering herd), FIFO, каждый трогает только свою кеш-линию. Внутри одного vcore
contended lock не ждёт вовсе — прямой handoff владельцу с донорством приоритета.

**Condvar**: seq-счётчик поколений:

```
wait(cv, m): s = cv->seq; unlock(m); wait(&cv->seq, s); lock(m);
notify_all:  cv->seq++      // одна запись будит ВСЕХ umwait'еров на линии —
                            // аппаратный broadcast, бесплатный по природе
notify_one:  запись в wait-word первого из списка ожидающих (MCS-стиль)
```

## 5. SPSC-очередь: разбор целевого сценария (два ядра)

В steady state (очередь непуста) мьютекс не контендится (SPSC: стороны работают с
разными концами; можно per-end/biased locks), condvar wait не зовётся. Весь вопрос
— цена «очередь опустела → пришёл элемент»:

```
Producer:  запись элемента в ring
           store tail (release = обычный mov на x86)   <- это одновременно и notify!
           (опц.) cldemote линии tail                  <- вытолкнуть в LLC навстречу читателю

Consumer (пусто):
           umonitor(&tail); tail всё ещё пуст? -> umwait
           ...store продюсера в tail будит монитор...
```

Монитор вооружён прямо на `tail` — продюсеру не нужно знать, что кто-то спит: его
штатная запись хвоста и есть пробуждение. Бюджет «push → pop вернулся»: перенос
линии (~40–100 нс) + выход из C0.1 ≈ **150–300 нс**; спиновый буфер ~100–200 нс.
Паритет по порядку, ноль сожжённых циклов, на SMT-паре umwait выигрывает у спина.

Обязательные штрихи (Rigtorp): head/tail на разных линиях, кешированные локальные
копии индексов (режут когерентный трафик ~вдвое), данные inline в ring.

Условия честного бенча: пара закреплена на ядрах одного LLC/CCX;
`IA32_UMWAIT_CONTROL` выставлен ядром щедро; C0.2 vs C0.1 — ручка латентность/энергия.

## 6. Один core: паритет с корутинами

Ярус 1 не нужен: второй поток на том же vcore, ждать «снаружи» бессмысленно.

- `cv.wait()` → положить себя в список ожидающих, свитч на следующий green thread (~25 нс).
- `notify` → перевести в runnable; вариант `notify_and_switch` — немедленный handoff
  для минимальной латентности.
- Contended mutex на одном vcore → прямой handoff владельцу (спин бесполезен по
  определению) с донорством приоритета.

«notify → consumer проснулся» = один корутинный свитч, десятки нс, 0 сисколов,
кеши/TLB не тронуты. Семантика корутин через стандартный API — код переносим.

## 7. Ярусы 2–3: парковка в ядре без IPI

Когда umwait истёк (объект реально простаивает):

- `upark(addr, expected, deadline)`: ядро валидирует слово и, **если у ядра нет
  другой работы, idle-петля делает mwait на том же user-слове**. Запись будящего
  будит физядро напрямую — по-прежнему ни сискола, ни IPI у будящего. (Тот же
  трюк, что TIF_POLLING в Linux-idle, но нацеленный на user-слово; ср. «Adios to
  Busy-Waiting», EuroSys'25.)
- Несколько спящих на ядре → mwait на **per-CPU doorbell-линии**, отображённой в
  user-память; будящий видит `sleep_state = KERNEL{cpu}` (соседнее поле) и пишет
  ещё и в doorbell. Два mov вместо одного — всё ещё без ядра.
- Только ярус 3 (ядро занято другой нитью) требует `uwake(tid)`/IPI — честный
  компромисс: раз ядро занято, латентность всё равно определяется вытеснением.

**Критичная гонка яруса 2** (у будящего `store data; load sleep_state` — StoreLoad,
load может уйти вперёд и увидеть устаревший AWAKE): лечится **асимметричным
барьером** — ядро при `upark` шлёт один IPI ядру потенциального будящего (дренаж
store buffer), после чего перепроверяет слово и только потом усыпляет. Цена — на
медленном пути спящего; fast path будящего остаётся без fence/lock-префикса
(трюк `sys_membarrier`, встроенный в парковку).

## 8. Кеш-политика (сквозной принцип «не сбрасывать лишний раз»)

- Жёсткий аффинитет vcore ↔ физядро; work stealing только с idle-ядер и только
  холодных задач.
- Никаких общих runqueue-структур между ядрами — шедулерная мета per-core.
- Green-свитч не сохраняет FPU; kernel-свитч — XSAVES с init/modified-оптимизацией
  (AVX-512 лениво).
- PCID / без смены CR3 между нитями процесса; в Talaria без KPTI сискол
  `upark`/`swtchto` — ~сотня циклов.
- `cldemote` после записи tail (SPR+) — линия едет в LLC навстречу читателю;
  `tpause` вместо `pause` в ярусе 0.
- Спящий в umwait не загрязняет ничьи кеши — в отличие от спиннера, который
  молотит линию и SMT-соседа.

## 9. Поверхность API ядра

```
vcore_request(n) / upcall-вектор       — M:N, активации (этап 4)
upark(addr, expected, tsc_deadline)    — ярусы 2–3, с асимметричным барьером
uwake(tid | addr)                      — холодный путь
swtchto(tid)                           — направленный handoff мимо runqueue (наследие switchto/UMCG)
per-CPU doorbell page (RW map)         — пробуждение idle-ядер записью
wrmsr IA32_UMWAIT_CONTROL              — разрешить глубокие/долгие umwait
CR4.MONITOR для гостевых фич           — детект waitpkg/monitorx при бооте
```

## 10. Автоматический подхват Linux/C/C++-приложений

pthread mutex/cond/rwlock/sem, C11 thrd, std::mutex/std::condition_variable
(libstdc++/libc++ маппятся на pthreads) — всё в итоге ходит в futex. Отсюда три
слоя, от «бесплатно всем» к «максимум для своих»:

1. **Ядро: быстрый futex как есть** — все существующие бинари (static и dynamic,
   musl и glibc) уже зовут futex-сисколы. Ускоряем сам futex: дешёвый вход (уже
   так: без KPTI/митигаций), **directed wake** (FUTEX_WAKE передаёт CPU ждущему
   handoff'ом вместо runqueue), **mwait-idle на futex-слове** паркующегося (ярус 2
   средствами ядра). Выигрыш всем приложениям без единого изменения в user space;
   wake остаётся сисколом, но путь пробуждения падает с мкс до сотен нс.
2. **Патченный musl на образе**: Talaria владеет диском и путём PT_INTERP —
   заменяем `/lib/ld-musl-x86_64.so.1` + `libc.so` на свой билд musl, где
   внутренние `__futexwait`/`__wake`/`__timedwait` идут через ярусы 0–1 (tpause →
   umonitor/umwait → syscall-fallback). Все **динамические** бинари (PostgreSQL,
   MariaDB, MongoDB и прочие musl-сборки) подхватывают автоматически, без
   пересборки: 0 сисколов в fast path у обеих сторон. glibc-динамику не патчим —
   её покрывает слой 1 (опц. LD_PRELOAD-шим pthread_* поверх).
3. **Нативная libtsync для своих ячеек**: C/C++ API wait/notify,
   mutex MCS+umwait, condvar, SPSC; на этапе 4 — green threads (M:N vcore),
   same-core condvar = корутинный свитч. Drop-in обёртки под pthread-сигнатуры —
   можно подложить и через LD_PRELOAD.

Матрица подхвата: static+glibc → слой 1; dynamic+musl → слои 1+2 (полный fast
path); наши ячейки → слои 1+2+3.

## 11. Стенд/QEMU: что нужно от виртуализации

- `-overcommit cpu-pm=on` — иначе mwait/umwait гостя = VM-exit и вся идея умирает.
- `-cpu host,+waitpkg` — WAITPKG в госте (KVM поддерживает passthrough); проверить
  `waitpkg` в /proc/cpuinfo **хоста**. Если хост-CPU без WAITPKG (до
  Tremont/Alder Lake): на AMD Zen есть user-mode `monitorx/mwaitx` (таймаут из
  коробки); иначе fallback — ярус 2 (`upark` + ring0-mwait) с дешёвым сисколом
  только у спящего; будящий всё равно без сискола.
- Матрица детекта в ядре при бооте: WAITPKG → umwait; MONITORX → mwaitx ring3;
  иначе — upark-fallback. Печать выбранного режима в boot-лог.
- **ivshmem-вишенка для харнеса (§8 TODO)**: ivshmem-страницы — обычная
  когерентная память хоста ⇒ umonitor/umwait поверх ivshmem может дать
  **меж-ВМ doorbell без virtio-kick и без vmexit** — прямой ответ на упор
  zero-syscall пути в лимит virtio-kick. Отдельный эксперимент.

## 12. Верификация и бенчи

1. Микробенч ярусов: same-core handoff (цель ≤50 нс), cross-core umwait wake
   (цель ≤300 нс), сравнение со спином и с futex на Linux на том же железе.
2. SPSC mutex+condvar vs спиновый ring: throughput и p50/p99 «push→pop» на паре
   ядер одного LLC (и отдельно на HT-паре — там umwait должен выиграть у спина).
3. Один vcore: SPSC через condvar vs ручные корутины (boost::context-стиль).
4. pthread condvar ping-pong на немодифицированном musl-бинаре: до/после слоя 1,
   затем слоя 2.
5. Регресс-гейт: test.sh + validate.sh зелёные, БД-нагрузки (PG/MariaDB/MongoDB)
   живы на патченном musl.
6. Эксперимент umwait-поверх-ivshmem в харнесе челленджа.

## 13. Реализация: файлы, тесты, инварианты

Ядро: `sync.rs` (детект-матрица, `kwait_once` MONITOR→recheck→sti;mwait, реестр
ждущих WADDR/WOKEN/WDEADLINE, `wait_on`/`wake_addr`, TSC-deadline), `percpu.rs`
(GS.base per-CPU без swapgs), `sched.rs` (CPU_OF-аффинити, `ap_loop` pinned-задач,
миграция `migrate_current_to`, HINT/`yield_to`/`yield_now`), `syscall.rs` (futex:
WAIT/WAKE/WAKE_OP/REQUEUE; `sched_setaffinity` 203 = миграция; `swtchto` 4600),
`idt.rs` (WAKE_VECTOR 0x43 = EOI-only), `smp.rs` (AP_MODE, EFER.NXE на AP).
User: `kernel/user/tsync/` — tsync.[ch] (wait-word/mutex/MCS/condvar/SPSC/MPSC),
fiber.[ch] (green threads), tsynctest/syncbench/fiberbench, musl-1.2.5-talaria.patch
+ build-musl.sh (патченный ld-musl → user/dyn/, слой 2 автоподхвата).
Тесты (test.sh): `sync` (ring0-селфтест + SYNC-MB замер membarrier), `smp2`
(миграция+параллельность), `tsync` (10 проверок: mutex/MCS-стресс, condvar
ping-pong и broadcast BSP+AP, SPSC cap 64/2 cross/same-core, ПЕРЕНОСИМЫЙ
протокол kfast=0 — ветка Linux, timedwait BSP/AP), `syncbench` (best-of-5 +
счётчики slow-путей), `futexlat`, `fiber`, `dynsync` (динамический бинарь на
патченном ld-musl), `shmwake` (две ВМ, ivshmem), fastflood (батч virtio-notify,
ассерт амортизации kick).

Инварианты/ограничения текущей реализации:
- GS ядерный per-CPU: user-коду нельзя менять GS (`arch_prctl(ARCH_SET_GS)` →
  EINVAL; загрузка селектора %gs в ring 3 сломает нить — не делает никто из libc).
- Миграция: только СВОЯ нить, только BSP→AP (обратно нет); AP — run-to-completion
  (без таймера и вытеснения), одна задача на AP за раз; сигналы на AP-нити не
  доставляются (доставка — BSP-пути).
- Сискол-поверхность AP-нити: futex/clock/write(serial)/mmap/exit проверены;
  файловые/сетевые сисколы с AP не сериализованы с BSP — не звать (бенч-нити).
- clone/fork с AP-нити не тестированы — создавать нити с BSP.
- futex: private-семантика в пределах процесса (кросс-процессный futex по общей
  памяти не матчится — у DB-нагрузок не используется).
- Таймауты futex: BSP — гранулярность тика; AP — TSC-deadline (точно), без него
  timed-wait на AP деградирует в pause-поллинг.

## 14. Ссылки

- UMCG / User-Managed Concurrency Groups: lwn.net/Articles/876671, lwn.net/Articles/879398
- FUTEX_SWAP / Google Fibers (switchto, ~200 нс свитч): lwn.net/Articles/823745
- umwait/tpause: lwn.net/Articles/790920; Intel «MONITOR/UMONITOR Performance Guidance»
- Scheduler activations (Anderson et al., 1992); Windows UMS
- Shenango (NSDI'19), Caladan (OSDI'20), Junction (NSDI'24), Concord (SOSP'23) —
  мкс-планирование, ядро-байпас
- «Adios to Busy-Waiting» (EuroSys'25) — mwait-пробуждения мкс-масштаба
- Rigtorp: «Optimizing a ring buffer for throughput» (cached indices, false sharing)
- FlexSC (OSDI'10) — косвенная цена сисколов/свитчей для кешей
- seL4 fastpath IPC (~150–300 циклов) — образец same-core handoff
