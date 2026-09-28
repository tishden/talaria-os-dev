# PLAN-B2 — Track B: от игрушечного сервера к измеренной платформе приложений

Хребет (выбран пользователем): **тезис-кульминация**. Порядок: конкурентность →
epoll → zero-syscall путь → измеренный A/B против Linux (финал). Хранилище — если
останется время. Всё проверяется локально в QEMU. Каждая фаза = коммит + запись в
docs/POC-RESULTS.md. Фазы почти независимы: застряла — перехожу к следующей.

Автономный забег на 6-8 часов. Этот файл — источник истины по прогрессу (обновляю
раздел «Прогресс» после каждого шага, чтобы пережить компакцию контекста).

## Фаза 0 — регресс-сеть (~40 мин)
- Скрипт kernel/test.sh: бутит все режимы в QEMU, ассертит вывод.
  Режимы: test=user/musl/printf/crypto/ipi, mode=smoltcp/trade/httpd/server.
- Fault-injection на socket-пути: битый пакет, RST посреди запроса, частичный read,
  запрос без тела. Сервер не должен падать/виснуть.
- Deliverable: `kernel/test.sh` зелёный. Коммит.

## Фаза 1 — конкурентность (~1.5 ч)
- netsys: пул из N tcp-сокетов (напр. 8). listener-pool: держим K сокетов в Listen
  на одном порту; accept() отдаёт первый активный, доводит пул новым listener'ом.
- fd-таблица: fd -> SocketHandle. accept возвращает разные fd; read/write/close по-fd.
- Проверка: 3+ параллельных socat, удерживаемые открытыми, чередующийся ввод.
- Deliverable: конкурентный сервер. Коммит.

## Фаза 2 — событийный цикл (~1.5 ч)
- Сисколы: epoll_create1(291), epoll_ctl(233), epoll_wait(232)/epoll_pwait(281),
  ppoll(271)/poll(7). netsys отдаёт READY per-handle (can_recv/can_send/closed).
- accept4 неблокирующий (O_NONBLOCK/EAGAIN), fcntl(72) заглушка для флагов.
- Запуск немодифицированного event-driven сервера (epoll-цикл на C/musl).
- Опционально: clone(56)+futex(202)+set_robust_list для musl-pthreads (если время).
- Deliverable: epoll-сервер в ring 3. Коммит.

## Фаза 3 — zero-syscall быстрый путь (ГВОЗДЬ, ~1.5 ч)
- Маппинг rx/tx virtio-колец + payload-DMA в user-space (U/S-биты на страницы кольца).
- Userspace-lib talaria_net.h: user поллит avail/used-кольца, читает пакет, пишет tx,
  бампает индексы — ВСЁ в ring 3, ноль сисколов на хот-пасе (kick — опционально MMIO
  или poll-mode без kick). Один системный вызов на сетап (mmap-регистрация кольца).
- Демо: userspace echo/trade-сервер поверх talaria_net.h.
- Deliverable: zero-syscall сервер. Коммит.

## Фаза 4 — измерение (КУЛЬМИНАЦИЯ, ~1.5 ч)
- A/B харнесс: одинаковая нагрузка (UDP-эхо/HTTP) на:
  (a) Linux-гость baseline (переиспользовать инфру v0.2),
  (b) Talaria socket-путь (mode=server),
  (c) Talaria zero-syscall путь (Фаза 3).
- Нагрузчик с хоста меряет rtt в TSC-циклах; в госте — сисколов/запрос, ctx-switch.
  Гистограмма хвостов (p50/p99/p999/max).
- Deliverable: docs/BENCH-B.md с таблицей и хвостами. Обновить §дизайн-дока. Коммит.

## Фаза 5 — хранилище (обязательно, по запросу пользователя, ~1-1.5 ч)
- virtio-blk драйвер (по образцу virtio-net). Минимальная read-only FS или raw-блоки.
- Сисколы open(2)/openat(257)/read/close/fstat/lseek для файлов с диск-образа.
- Демо: грузить srv-бинарь/конфиг с -drive вместо include_bytes!.
- Deliverable: файловый ввод. Коммит.

## Инварианты
- Не ломать существующие режимы (Фаза 0 харнесс ловит регресс).
- Каждый рабочий шаг — отдельный коммит с записью в POC-RESULTS.
- Сборка: rustup run nightly cargo -Z json-target-spec build --release.
- musl-бинари: docker talaria-musl, -Ttext-segment=0x10000000.
- QEMU: docker talaria-qemu2, microvm, tap0 10.0.0.1/24, гость 10.0.0.2.

## Прогресс
- [x] Фаза 0 — регресс-сеть (test.sh, 8/8 зелёных, fault-injection)
- [x] Фаза 1 — конкурентность (пул из 8 сокетов, listener-pool, 6/6 burst)
- [x] Фаза 2 — событийный цикл (epoll_create1/ctl/wait, accept4 NONBLOCK, fcntl; 6/6)
- [x] Фаза 3 — zero-syscall путь (fastpath.rs ячейка + fastapp.c, 3/3 UDP-эхо)
- [x] Фаза 4 — измерение (BENCH-B.md: сискол 28нс, reflect 60нс, AES 111нс, RTT A/B)
- [x] Фаза 5 — хранилище (virtio-blk + TFS1 + open/read/lseek/fstat/close; бинарь с диска)
