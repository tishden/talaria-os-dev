#!/usr/bin/env python3
"""A job for the `tlgen --serve` generator: an argument line over TCP, the report as JSON.

  job.py <address> <port> <report file> <tlgen arguments...>

Three rules that make this a separate file rather than a function inside the runner:

* A CONNECTION DROP IS NOT THE END OF THE RUN. Before, `ConnectionResetError` flew out
  as a traceback, the point was lost, and the next one ran into a timeout: one reset
  stopped the whole ladder. Now the point is MARKED: a stub with `schema: 1`,
  `counts.measured: 0` and the reason in `error` is written to the report file,
  and the summary prints it with dashes rather than staying silent. The exit code is always 0,
  except for an error in its own arguments.
* THE TIMEOUT IS PROPORTIONAL TO THE JOB rather than a quarter-hour constant: one hung
  measurement with the constant stopped the run for 15 minutes, and on a full ladder
  that meant a day instead of an hour.
* THE REPORT IS CHECKED ON THE SPOT: it must parse as JSON and contain
  `counts.sent` — otherwise the point is marked as on a drop. A file that is
  "almost JSON" does not enter the summary and means nothing.
* THE PATH TO THE GENERATOR IS NOT PART OF THE MEASUREMENT, and its breakage must not cost a point.
  `JOB_RETRY_S=<s>` (default 0 — as before) gives a retry budget: a connection refusal
  and a connection reset are retried with a pause until the budget runs out, and only
  then is the point marked. Why: on EC2 two test cases (redis `mixed`, nginx
  `file`) were lost ENTIRELY — "Connection refused" six times in a row — and
  the analysis went into the kernel (the listener's backlog, reactivation after RST), while the culprit was
  the VPN on the runner machine: it reconnected for 2-4 minutes, and during that time its
  client refused connections itself (and reset live ones itself). The generator's accept
  counter did not grow by a single SYN over the refusal window. Locally the
  generator is on the same machine and no retry is needed there — the budget is set by the EC2 runner.

* ARGUMENT BOUNDARIES SURVIVE THE TRIP. The arguments are glued into a string, and
  the receiver splits it on spaces (`split_args` in tlgen.c, which understands '...' and
  "..."). An argument with a space inside — `--query 'SELECT 1'` in mysql and
  postgresql — used to travel bare, the receiver saw `SELECT` and `1` as two
  arguments and answered `rc=2, the job did not parse`: the whole ladder of both databases on
  EC2 consisted of stubs (`ec2all-20260905-133810`, mysql — 20 points of 20),
  while the ceilings were still computed (they went another way and carried their quotes with them).
  Now such an argument is quoted here, once, for all paths.

The self-check is `bench/suite/selfcheck.sh`.
"""
import json
import os
import socket
import sys
import time


def quote(tok):
    """An argument as the receiver's `split_args` will parse it: without spaces and
    quotes — as is; otherwise in single quotes (in double ones if it contains
    a single quote; the receiver cannot do both at once — such an argument is split)."""
    if not any(c in tok for c in " \t'\""):
        return tok
    if "'" not in tok:
        return "'" + tok + "'"
    if '"' not in tok:
        return '"' + tok + '"'
    raise SystemExit("job.py: an argument with both kinds of quotes cannot be passed: %r" % tok)


def argval(toks, name, default=None):
    for i, a in enumerate(toks):
        if a == name and i + 1 < len(toks):
            return toks[i + 1]
    return default


def stub(toks, error):
    """A report stub for a point that did not happen — the same shape as tlgen's."""
    z = {"n": 0, "min_ns": 0, "mean_ns": 0.0, "max_ns": 0, "p50": 0, "p75": 0, "p90": 0,
         "p99": 0, "p99_9": 0, "p99_99": 0, "p99_999": 0}
    return {
        "schema": 1, "name": argval(toks, "--name", ""), "stand": argval(toks, "--stand", "?"),
        "proto": argval(toks, "--proto", "?"), "tags": argval(toks, "--tags", ""),
        "cfg": {"rate": int(float(argval(toks, "--rate", "0"))), "conns": int(argval(toks, "--conns", "0")),
                "duration": float(argval(toks, "--duration", "0"))},
        "counts": {"sent": 0, "done": 0, "measured": 0, "lost": 0, "timeouts": 0, "conn_errors": 0,
                   "app_errors": 0, "backlog_max": 0, "backlog_ns": 0, "overload": False},
        "throughput_rps": 0.0, "rate_achieved_pct": 0.0,
        "syscalls": {"send": 0, "recv_data": 0, "recv_empty": 0, "epoll_wait": 0, "useful_per_req": 0.0, "all_per_req": 0.0},
        "latency": {"open_loop": dict(z), "service": dict(z)},
        "timeline": [], "hist_ns": [], "rc": 1, "error": error,
    }


def parse_report(buf):
    """The first JSON object from the answer. A tail after it (an old tlgen appended
    `{"error": N}` on a separate line) is no reason to lose the report, but it is a reason
    to say so."""
    text = buf.decode("utf-8", "replace")
    r, end = json.JSONDecoder().raw_decode(text.lstrip())
    tail = text.lstrip()[end:].strip()
    if tail:
        r.setdefault("error", "tail after the report: %.80s" % tail)
    return r


def check(r):
    """What a report must contain to enter the summary."""
    if r.get("schema") != 1:
        return "schema != 1"
    if "counts" not in r or "sent" not in r["counts"]:
        return "no counts.sent"
    if "latency" not in r or "open_loop" not in r["latency"]:
        return "no latency.open_loop"
    return None


def ask(gen, port, args, timeout):
    """One request to the generator: (report, None) or (None, reason)."""
    try:
        s = socket.create_connection((gen, port), timeout=30)
    except OSError as e:
        return None, "the generator is unreachable: %s" % e
    s.settimeout(timeout)
    buf = b""
    try:
        s.sendall((args + "\n").encode())
        while True:
            c = s.recv(65536)
            if not c:
                break
            buf += c
    except OSError as e:
        return None, "the connection to the generator dropped: %s" % type(e).__name__
    finally:
        s.close()
    return buf, None


def main(argv):
    if len(argv) < 4:
        print("job.py <address> <port> <report file> <tlgen arguments...>", file=sys.stderr)
        return 2
    gen, port, out, toks = argv[0], int(argv[1]), argv[2], argv[3:]
    args = " ".join(quote(t) for t in toks)

    def mark(why):
        # THE EXIT CODE DISTINGUISHES "THE POINT WAS NOT TAKEN" FROM "THERE IS NO GENERATOR".
        #
        # A point stub is normal behaviour (a VPN drop, garbage in the answer): the runner
        # goes on. But A DEAD RECEIVER is not one point but all the
        # remaining ones: each spends its retry budget (600 s) and fails the
        # same way. On one scoreboard mysql spent 4.9 hours on this and gave 12
        # judgeable points out of 40 — both sides stopped answering in turn.
        # Code 3 lets the runner abandon the rest of this side's ladder.
        print("     the point was not taken: %s" % why)
        with open(out, "w") as f:
            json.dump(stub(toks, why), f, indent=1, ensure_ascii=False)
        return 3 if why.startswith("the generator is unreachable") else 0

    budget = float(os.environ.get("JOB_RETRY_S", "0"))
    deadline = time.monotonic() + budget
    attempt = 0
    while True:
        attempt += 1
        buf, why = ask(gen, port, args, float(argval(toks, "--duration", "60")) + 120)
        if why is None:
            break
        left = deadline - time.monotonic()
        if left <= 0:
            if attempt > 1:
                why += " (retries %d, budget of %.0f s exhausted)" % (attempt - 1, budget)
            return mark(why)
        pause = min(5.0, left)
        print("     %s — retrying in %.0f s (%.0f s of budget left)" % (why, pause, left))
        time.sleep(pause)
    if not buf.strip():
        return mark("the generator closed the connection without answering")
    try:
        r = parse_report(buf)
    except ValueError as e:
        return mark("the generator answered with something other than a report: %s: %.200s" % (e, buf.decode("utf-8", "replace")))
    bad = check(r)
    if bad:
        return mark("the report is incomplete (%s): %.200s" % (bad, buf.decode("utf-8", "replace")))
    with open(out, "wb") as f:
        f.write(buf)
    lat = r["latency"].get("service") or r["latency"]["open_loop"]
    ol = r["latency"]["open_loop"]
    if int(r["counts"].get("measured", 0)) == 0:
        print("     not a single measured answer: %s" % r.get("error", "no reason given"))
        return 0
    print("     %.0f req/s  service p50=%.1f p99=%.1f p99.9=%.1f p99.99=%.1f µs  debt p99=%.1f µs%s"
          % (r["throughput_rps"], lat["p50"] / 1e3, lat["p99"] / 1e3, lat["p99_9"] / 1e3, lat["p99_99"] / 1e3,
             max(0, ol["p99"] - lat["p99"]) / 1e3,
             "  OVERLOAD" if r["counts"].get("overload") or r.get("rate_achieved_pct", 100) < 98 else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
