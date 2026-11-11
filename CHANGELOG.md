## [0.3.2] - 2026-11-11

### 🚀 Features

- *(sync)* Implement end-to-end encryption for sync
- *(grid)* Heatmap display-list items and per-day color ranges

### 🐛 Bug Fixes

- *(sync)* Validate key overrides and improve key diagnostics
- *(sync-server)* Require a key id and encryption on every push
- *(sync)* Retire cascaded outbox cells on delete

## [0.3.1] - 2026-11-07

### 🚀 Features

- `im -V` / `im --version` prints the version.
- Task creation uses `+`: `im +`, `im + <name>`, `im +<ref> <name>`,
  `im + %`, and `im + @`; recurring names require the `:<name>` marker.
- Trackers, at most one task reference, and task updates can be freely mixed
  in one entry, and `--quiet` works as a long flag.
- Sessions show a countdown chosen by `[session] countdown` (`timer`, `bar`,
  `dots`, `percent`, `minimal`); `bar` is the default.
- Text trackers take a configurable badge color
  (`badges.default_text_tracker_color`, `dark_grey` by default) instead of
  inheriting the darkest bin of the default ramp.
- New `notify` crate and CLI: cross-platform toasts and dialogs with bundled
  `bell`, `bowl` and `chime` sounds played through rodio.
- `im :tool datetime [expr]` resolves a date expression and prints it in a
  chosen format. `--format` accepts presets (`civil`, `utc`, `short`, `unix`,
  `iso`, `date`, `time`, `dmy`, `mdy`), a time zone (IANA name, unique case-insensitive
  fragment, or fixed offset such as `+08:00`), or a custom `strftime`
  pattern. A resolved zone reports its name and offset on stderr unless
  quiet.
- Bundled mood palette variants: earth, nordic, jewel, bloom, and pastel.
