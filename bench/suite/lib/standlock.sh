# Sourced: ONE RUN PER STAND. `stand_lock` takes an exclusive lock on $STAND_DIR (the stand's
# state: stand.env, the AMI log, the consoles) and refuses when another run holds it, naming
# that run. Why: on 2026-10-07 two queued A/B chains ran at once on one stand — the second
# brought its MariaDB AMI up through the same terraform state in the middle of the first one's
# MongoDB run, which then measured a MariaDB server with a generator that was no longer there,
# and every later run failed on terraform's lock. Nothing said "a second run is here".
# A nested call (a run's own stand-up/stand-down) sees STAND_LOCK_HELD and goes on.
stand_lock() {
    [ "${STAND_LOCK_HELD:-}" = "$STAND_DIR" ] && return 0
    mkdir -p "$STAND_DIR"
    exec 9>>"$STAND_DIR/.run.lock"
    if ! flock -n 9; then
        echo "REFUSAL: another run holds the stand $STAND_DIR — $(cat "$STAND_DIR/.run.owner" 2>/dev/null || echo '?')"
        echo "         (one run per stand: wait for it, or give this one its own STAND_DIR)"
        exit 1
    fi
    echo "pid $$, $0 $*, since $(date '+%Y-%m-%d %H:%M:%S')" > "$STAND_DIR/.run.owner"
    STAND_LOCK_HELD=$STAND_DIR; export STAND_LOCK_HELD
}
