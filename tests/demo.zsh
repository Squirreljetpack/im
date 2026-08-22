#!/usr/bin/env zsh
# demo.zsh — spin up a throwaway environment for `im`:
#
#   - a temporary state dir holding a SQLite DB seeded with a year's worth of
#     mood entries (some pulled from model/mood_journal_dataset.csv, some
#     hand-written). 0-4 days per month have no entries.
#   - a temporary config dir with a custom config.toml.
#
# It then exports the env vars `im` honors (IM_CONFIG_DIR for the config dir,
# XDG_STATE_HOME for the state dir -> im.db / log file) and replaces the shell
# (`exec -l zsh`) so everything you run inside lands in the sandbox.
#
# Usage: ./demo.zsh [path-to-im-binary]
#        (default: target/release/im, falling back to target/debug/im)

set -euo pipefail

emulate -L zsh

ROOT="${0:A:h}"
BIN="${1:-}"
if [[ -z "$BIN" ]]; then
  if [[ -x "$ROOT/target/release/im" ]]; then
    BIN="$ROOT/target/release/im"
  elif [[ -x "$ROOT/target/debug/im" ]]; then
    BIN="$ROOT/target/debug/im"
  else
    print -u2 "demo.zsh: no im binary found under target/; build one or pass a path"
    exit 1
  fi
fi

command -v sqlite3 >/dev/null 2>&1 || { print -u2 "demo.zsh: sqlite3 is required"; exit 1 }
command -v gdate >/dev/null 2>&1 && DATE=gdate || DATE=date   # macOS safety net

DEMO_DIR="$({ mktemp -d "${TMPDIR:-/tmp}/im-demo.XXXXXX" } 2>/dev/null)" \
  || DEMO_DIR="$(mktemp -d /tmp/im-demo.XXXXXX)"
CONFIG_DIR="$DEMO_DIR/config"
STATE_DIR="$DEMO_DIR/state"
mkdir -p "$CONFIG_DIR" "$STATE_DIR/im"

# Debug builds use im.dev.db, release builds im.db — seed both, they're tiny.
DB_REL="$STATE_DIR/im/im.db"
DB_DEV="$STATE_DIR/im/im.dev.db"

# ---------------------------------------------------------------- mood pool --
typeset -a MOODS
MOODS=(
  calm and focused
  bright morning energy
  sluggish but getting there
  quietly content
  anxious about deadlines
  rested and clear-headed
  restless evening thoughts
  proud of small wins
  foggy and unmotivated
  warm after a good walk
  tense shoulders busy mind
  grateful sleepy satisfied
  low hum of dread
  light and social today
  drained after meetings
  hopeful about tomorrow
)

# Pull extra mood strings from the dataset (input_text = 3rd field onward).
if [[ -f "$ROOT/model/mood_journal_dataset.csv" ]]; then
  MOODS+=("${(@f)$(tail -n +2 "$ROOT/model/mood_journal_dataset.csv" | cut -d, -f3- | sed '/^$/d')}")
fi

# ------------------------------------------------------------------- config --
# Minimal custom config; every section of im's config is optional, unknown
# keys are rejected, so only touch keys that exist.
cat >"$CONFIG_DIR/config.toml" <<'TOML'
[preview]
show_last_when_done = true
named_months = true

[tasks_view]
persist_pending_seconds = 300

[badges]
journal_badge = "·"
TOML
# Debug builds read dev.toml instead of config.toml — same content.
cp "$CONFIG_DIR/config.toml" "$CONFIG_DIR/dev.toml"

# --------------------------------------------------------------------- seed --
SQL="$DEMO_DIR/seed.sql"
: >"$SQL"

esc_sql() { print -r -- "${1//\'/\'\'}"; }

now=$($DATE +%s)
n_moods=${#MOODS}
skipped_days=0
inserted=0

typeset -A skips_left

# Walk backwards over the last 365 days.
for (( off = 365; off >= 1; off-- )); do
  day=$($DATE -d "@$(( now - off * 86400 ))" +%Y-%m-%d)
  ym=${day%-??}

  # Per month: allow 0-4 entry-less days, distributed through the month.
  if [[ -z "${skips_left[$ym]:-}" ]]; then
    skips_left[$ym]=$(( RANDOM % 5 ))
  fi
  if (( skips_left[$ym] > 0 )) && (( RANDOM % 8 == 0 )); then
    (( skips_left[$ym]-- , skipped_days++ ))
    continue
  fi

  # 1-3 mood entries per kept day.
  for (( n = 1 + RANDOM % 3; n > 0; n-- )); do
    h=$(( 7 + RANDOM % 16 )); m=$(( RANDOM % 60 )); s=$(( RANDOM % 60 ))
    ts=$($DATE -d "$day $h:$m:$s" +%s)
    mood="${MOODS[RANDOM % n_moods + 1]}"

    # Every ~5th entry is a timed session (duration column, seconds).
    if (( RANDOM % 5 == 0 )); then
      dur=$(( (25 + RANDOM % 3 * 15) * 60 ))   # 25m / 40m / 55m
      body=""
      (( RANDOM % 4 == 0 )) && body="felt ${mood} during the block"
      print -r -- "INSERT INTO mood (mood, body, time, embedding, score, duration) VALUES ('$(esc_sql "$mood")', '$(esc_sql "$body")', $ts, NULL, NULL, $dur);" >>"$SQL"
    else
      print -r -- "INSERT INTO mood (mood, body, time, embedding, score, duration) VALUES ('$(esc_sql "$mood")', '', $ts, NULL, NULL, NULL);" >>"$SQL"
    fi
    (( inserted++ ))
  done
done

# The mood table matches im's CREATE TABLE IF NOT EXISTS exactly; im creates
# its other tables (tracker, todos, ...) on first startup.
for db in "$DB_REL" "$DB_DEV"; do
  sqlite3 "$db" 'CREATE TABLE IF NOT EXISTS mood (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      mood TEXT NOT NULL,
      body TEXT NOT NULL DEFAULT '"''"',
      time INTEGER NOT NULL DEFAULT (unixepoch()),
      embedding BLOB,
      score REAL,
      duration INTEGER
  );'
  sqlite3 "$db" <"$SQL"
done

print "demo db ready: $inserted mood entries across the past year ($skipped_days entry-less days)"
print "  release db: $DB_REL"
print "  debug   db: $DB_DEV"
print "  config dir: $CONFIG_DIR"
print "  binary    : $BIN"
print "(sandbox files live under $DEMO_DIR — delete it when done)"

# ------------------------------------------------------------- sandboxed env --
export IM_CONFIG_DIR="$CONFIG_DIR"     # custom config override (must exist)
export XDG_STATE_HOME="$STATE_DIR"     # state dir -> im.db / im.dev.db / log
                                       # note: affects other XDG-aware apps in
                                       # this shell too — that's the point.

exec -l zsh
