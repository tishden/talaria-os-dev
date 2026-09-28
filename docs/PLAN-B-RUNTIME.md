# PLAN-B-RUNTIME — широта рантайма Track B (bootable-ядро)

Цель: B гоняет всё более крупный реальный софт (путь к крупным C++-приложениям). В QEMU, безопасно.
Хребет выбран пользователем: широта рантайма.

## Фазы
1. **C++-рантайм** — запустить статическую C++-программу (libstdc++): static-init
   (.init_array), исключения (раскрутка стека, __cxa_*), TLS, operator new/delete,
   std::string/vector/iostream. Тест: C++ с vector/string/cout + try/catch.
2. **Полный набор сисколов** — заполнить пробелы под реальный софт: clock_gettime,
   nanosleep, poll/ppoll, sigaction/sigprocmask (реально), mprotect, mmap file-backed,
   getcwd, uname, readlinkat, sysinfo и т.п. Аудит через реальную программу.
3. **Многопроцессность: fork/exec** — отдельные адресные пространства (свои таблицы
   страниц на процесс, COW или полная копия), execve (загрузка нового ELF), wait4.
4. **Динамическая линковка** (опц.) — ld.so, PLT/GOT, запуск не-статических ELF.

## Прогресс

> **План ЗАКРЫТ целиком**, включая опциональную фазу 4. Документ оставлен как
> история постановки; сверка сделана прогоном регресса, а не по памяти.

- [x] Фаза 1 — C++-рантайм (STL, исключения, static-init — из коробки).
      Гейты: `cpp`, `cpp2`, `cpp3`, `cppdtor`.
- [x] Фаза 2 — полный набор сисколов: ~185 номеров Linux ABI. Мера полноты не
      список, а РЕАЛЬНЫЙ софт — PostgreSQL 16, MariaDB 10.11, MongoDB 4.0
      работают (`validate.sh` B10).
- [x] Фаза 3 — fork/exec. Гейты: `forkp` (fork + COW-рефкаунт в приватном AS),
      `exec`, `procs`, `procdisk`, `sigtest` (сигналы, fd-таблицы, wait4).
- [x] Фаза 4 — динамическая линковка. Гейты: `dyn` (ld-musl, PIE, dlopen libz),
      `dynglibc` (glibc 2.35 ld-linux, Ubuntu 22.04), `dynsync`.
