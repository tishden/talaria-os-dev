# PLAN-B-TOOL — Track B: от PoC к рабочему инструменту

Пункты 3,4,5 из разрыва PoC→продукт (по запросу пользователя). Core utils: НЕ делаем
полный coreutils (противоречит одно-задачной минимальной ОС + требует fork/exec/shell);
вместо этого control-console + gdb-stub + телеметрия (управление = control plane).

> **План ЗАКРЫТ целиком.** Документ оставлен как история постановки; каждый
> пункт ниже отмечен тем ГЕЙТОМ, который его держит сегодня (`kernel/test.sh`).
> Сверка сделана прогоном регресса, а не по памяти.

## Пункт 3 — Полнота рантайма (стабильность)
- [x] Реальное управление памятью: free-list с коалесингом (`mem::free_contig`),
      `munmap` реально снимает отображение. Гейты: `mem` (mmap/munmap reclaim),
      `uaf` (#PF после munmap — отображения действительно нет).
- [x] Фикс #DE на выходе (глобальные деструкторы / путь exit) — RELEASE-1.0 §1.3.
      Гейт: `cppdtor` (нить+condvar ПОСЛЕ return из main, затем `exit_group(0)`).
- [x] Убрать PoC-срезы: честный `/dev/null` + per-process fd-таблица.
      Гейт: `devnull` (write отброшен, read=EOF, close).
- [x] Стабильность на длинном прогоне. Гейты: `soak` (20k итераций churn, free
      до == free после), `restartmem` (дрейф между рестартами < 1 МБ) и
      `kernel/soak.sh` (30 мин СУБД под нагрузкой).

## Пункт 4 — Операторка
- [x] Манифест-запуск нагрузки — `cell.manifest` (`cellman.rs`), режимы
      `mode=cell`/`mode=proc`. Гейты: `manifest`, `cell`, `cell-plan`.
      С v1.9 манифест задаёт и раскладку по ядрам (USER-GUIDE §6.6).
- [x] Supervise/restart при падении ячейки — `supervise.rs` (setjmp/longjmp).
      Гейт: `restartmem` (3 экземпляра memhog, лимит перезапусков отработал).
- [x] Телеметрия ячейки в канал — `telemetry.rs`. Гейт: `telemetry`
      (pkts/p50/p99/gaps/restarts/uptime в канале).
- [x] Мини-консоль по serial — `console.rs`. Гейт: `console`
      (help/status/mem/ps/cores/dump/log).

## Пункт 5 — Наблюдаемость/отладка
- [x] gdb-stub по serial — `gdb.rs`. Гейт: `gdb-stub` (g/qfThreadInfo по RSP).
- [x] Crash-dump на panic/#PF/#GP — `idt.rs`. Гейт: `flightrec`
      (crash-dump + кольцо событий).
- [x] PMU-трейсинг (rdpmc) — `pmu.rs`. Гейт: `pmu` (cycles/instrs/IPC).
- [x] flight-recorder — `fr.rs`. Гейт: тот же `flightrec`.
