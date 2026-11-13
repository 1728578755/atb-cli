<div align="center">

# atb

A browser CLI for agents: a Rust daemon plus a Chrome MV3 extension bridge that drives the user's logged-in Chrome / Edge.

Tab scanning · Page reading · JS execution · Click and fill · User scripts · Screenshots / PDF · Cookies · Network capture · CDP · Multi-agent isolation

<p>
  <a href="https://github.com/1728578755/atb-cli"><img src="https://img.shields.io/badge/CLI-atb-2ea44f" alt="CLI atb"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-green" alt="License MIT"></a>
  <a href="https://github.com/1728578755/atb-cli"><img src="https://img.shields.io/badge/sys-win%2Fmac%2Flinux-0078D6?labelColor=0078D6&color=C0C0C0" alt="sys win/mac/linux"></a>
  <a href="https://github.com/1728578755/atb-cli/releases"><img src="https://img.shields.io/badge/release-v0.8.0-orange" alt="release v0.8.0"></a>
  <a href="https://github.com/1728578755/atb-cli/pulls"><img src="https://img.shields.io/badge/PRs-welcome-brightgreen" alt="PRs welcome"></a>
</p>

[Quick start](#quick-start) · [Install](#install) · [Capability map](#capability-map) · [Versions](#versions-and-compatibility) · [Security](#security-and-privacy) · [Docs](#documentation-map)

[中文](README.md) | English

</div>

## What it is

`atb` lets an agent use the browser you already have open: a long-running Rust daemon connects to the running Chrome / Edge through a Chrome extension, and each CLI call is a short-lived process that sends one command to the daemon.

Login state, cookies, installed extensions, and the current window stay as they are, because it is your window. One command can target several tabs at once; results come back in input order as JSON, and exit codes separate "the command is wrong" from "retry later", so scripts never parse error text.

The daemon stays resident; it exits after 300 idle seconds by default, but never while an extension is still connected, so consecutive operations do not handshake again for every command.

**What it is not**: not Selenium / Playwright (it never launches a throwaway automation profile and does not own the browser by default; only an explicit `atb launch --restart` restarts it); not a cloud browser (traffic between the CLI, daemon and extension stays on `127.0.0.1`; the pages you drive still talk to their own sites); not a stealth browser (it only reduces this tool's own traces and does not patch fingerprints).

## Quick start

```bash
# 1. Install the CLI — Node >= 18
npm i -g @g1728578755/atb

# 2. Load the extension: chrome://extensions → Developer mode → Load unpacked
#    pick the directory where chrome-extensions.zip was extracted (manifest.json sits directly inside);
#    the repo and npm package also ship the same directory as assets/atb_bridge

# 3. Open a page: a target command starts the daemon automatically
atb open https://example.com

# 4. Confirm the extension bridge
atb tabs --ids

# 5. Read the current page, or run JS in it
atb read --tab current --max-chars 4000
atb exec 'return document.title'

# For a full health check (status does not start the daemon; run the commands above first)
atb status
```

No browser restart is needed after loading the extension. When a command returns `ok:false`, read `error_code` and `hint` (some timeout cases only carry `error` / `reason`); `atb schema --json` gives an agent the arguments of top-level commands.

## How it differs from Selenium / Playwright

| Dimension | atb | Selenium / Playwright |
| --- | --- | --- |
| Session source | Reuses the user's logged-in Chrome / Edge | Starts a fresh profile with no login state |
| Concurrency | One command over many tabs | You write the concurrency and pooling yourself |
| Output | Compact JSON plus structured error_code / exit codes | Native objects and exceptions |
| Detection surface | Read / exec / wait do not attach; snapshot, plain-CSS click/fill, screenshots and PDF try to attach, with automatic fallback when refused (see below) | Automation markers and a debugger by default |
| Best fit | An agent driving the user's own browser | End-to-end suites built from scratch |

It does not replace a test framework: for isolated environments and hundreds of parallel cases, Playwright is the better tool.

## Capability map

| Domain | Representative command | What it does |
| --- | --- | --- |
| Target and environment | `atb focus` / `atb use` / `atb env` | Bring a tab forward, pin the default target, and see browser / extension / tabs / injections in one shot |
| Page reading | `atb read` | Main content, links, hidden nodes, iframes, truncated by a character budget |
| Page actions | `atb snapshot` / `atb measure` | Emits `@e1` handles; measures element rect, computed style and shadow path |
| Scripts and batches | `atb exec` / `atb inject` / `atb userscript` / `atb scrape` | Run JS, concatenate files, GM-compatible injection, userscript install page, cross-tab scraping |
| Network and debugging | `atb net` / `atb console` / `atb downloads` | Capture requests, mock responses, console history and tailing, download watching |
| Challenges and traces | `atb challenge` / `atb stealth` | Challenge detection and active solving; trace audit and scrubbing |
| Multi-browser and ops | `atb browsers` / `atb extensions` / `atb doctor` / `atb logs` | Multi-browser / Profile selection, installed extensions, diagnostics, logs |

## Install

### npm

```bash
npm i -g @g1728578755/atb
atb status
```

Prebuilt binaries come from `optionalDependencies` per platform, so no Rust toolchain is required. Write `atb` in new scripts.

### Build from source

```bash
cargo build --release
./target/release/atb status
```

Point `ATB_BIN` at the built binary to avoid mixing it with a global install.

### Load the extension

1. Download `chrome-extensions.zip` from the [latest Release](https://github.com/1728578755/atb-cli/releases/latest) and extract it into a fixed directory (the zip root is the extension itself, with `manifest.json` directly inside), or use `assets/atb_bridge` from the repo.
2. Open `chrome://extensions` (Edge: `edge://extensions`), enable developer mode, click "Load unpacked", and select the directory that directly contains `manifest.json`.
3. Keep at least one normal web page tab open — not only `about:blank` or `chrome://` pages.
4. **This release requires extension 5.5.0.** After you replace the extension directory the daemon asks the extension to reload on the next handshake; if the daemon is not running, click "Reload" on the extension page yourself — copying files without reloading keeps the old code running in pages.
5. Click the ATB toolbar icon to open the popup: connection status on top, then three tabs — Status / Data / Settings. The gear at the top right opens the **management page** (also reachable from `chrome://extensions` → Details → Extension options) where you can change the port, token, trust mode, theme, popup width and density, and inspect cookies, heartbeat and self-check results. To use `atb extensions`, click "About → Advanced permissions" on the management page and grant `management`; it is an optional permission, and the command returns `permission_required` until it is granted.

### Verify

```bash
atb tabs --ids           # run this first: starts the daemon and proves the bridge works
atb status               # then check health; running status first reports daemon_not_running
atb env                  # browser / Profile / extension / tabs / injections in one call
atb doctor               # use it when nothing connects: ports, token, daemon state
```

### Platform matrix

| Platform | Prebuilt | Notes |
| --- | --- | --- |
| macOS arm64 / x64 | Yes | — |
| Linux glibc x64 / arm64 | Yes | The local Chrome / Chromium must allow loading extensions; WSL 2 also uses this package |
| Windows x64 | Yes | Native Windows; do not use the win32 package inside WSL |
| Windows ARM64 | No | `cargo build --release`, then point `ATB_BIN` at the binary |
| Alpine / musl | No | Same as above, build it yourself |

WSL needs `WSL 2.0.0+`; on Windows 11 22H2+ set `networkingMode=mirrored` in `.wslconfig` so WSL can reach the bridge service on the host `localhost`.

## Core concepts

### Daemon

When the `atb` binary finds no daemon it starts a resident process that listens on `127.0.0.1:18767` and serves the HTTP API; later commands reuse that session. It exits after 300 idle seconds only when no extension is connected; while an extension stays connected it keeps running, with a 60-second reconnect grace after a disconnect. `ATB_IDLE_TTL` changes the idle value. Logs go to `~/.atb/logs/daemon.log`.

### Extension bridge

The extension opens a WebSocket to `127.0.0.1:18765`, using `extension_port` from `~/.atb/config.json`. The daemon sends commands, the extension runs them in the tab and returns JSON. `browser_label` in `status` / `tabs` / `browsers` carries the brand and full version (for example `Chrome 156.0.8078.79 · Windows · #cac3`), so parallel browsers are distinguishable at a glance. Reloading the extension or restarting the browser invalidates every tab id under that browser and yields `session_gone`: `open` again, or run `tabs` for a fresh list. `status` also returns `browser` / `browser_label` / `profile` / `browser_summary` and prints a one-line "→ operating on: …" summary to stderr.

### Agent identity

Resolution order: `--agent` → `ATB_AGENT` → terminal session id (`WT_SESSION` / `TMUX_PANE` / `VSCODE_IPC_HOOK_CLI`, …) → `default`. `open` only moves the caller's own default target and `cleanup` only closes tabs this agent opened, so several agents can share one daemon without fighting over a page.

### Target selection

`--tab` accepts a single id, `111,222`, `all`, `@1` / `@1-5`, `@latest`, `url:<substring>`, `title:<substring>`, `group:<title>`, `window:<id>`, and `current`. Multi-target runs execute concurrently, return in input order, and one failure does not affect the others; when writing files `--out` must contain `{tab}` or `{index}` or preflight rejects the command.

- `@latest` is the daemon's "most recently opened / connected" tab; `@1` is still the first row in `tabs` output, and the two are not interchangeable.
- `atb use <target>` pins the current agent's default target to one tab (multiple matches fail with `target_ambiguous` instead of silently picking the first); later commands without `--tab` use it.
- `atb focus --tab @latest` brings the tab to the foreground; `--no-window` activates the tab without stealing system window focus.
- Single-target page commands now carry top-level `resolved_target` (tab_id / session_key / url / title / browser_id / profile_id / active), `browser`, `profile`, `warnings` and `hint`. When the target is in the background, `warnings` contains `tab_hidden` / `timers_throttled` and the hint suggests `focus` (`ATB_HIDDEN_TAB_WARNING=off` disables it).
- The CLI prints "→ operating on: …" to stderr once by default; `ATB_BROWSER_BANNER=off` disables it and `always` prints it for every command.

### Protocols

Three versions must line up: the CLI ↔ daemon API protocol (currently **5**), the extension message protocol (currently **4**), and the extension file version (currently **5.5.0**). The `compatibility` field of `atb status` reports expected and actual values, with `fixes[]` when they disagree.

### New in 0.8.0 / extension 5.5.0

Top-level commands grew from 55 to **61**: `focus`, `use`, `env`, `extensions`, `measure` and `userscript` are new.

| Capability | Notes |
| --- | --- |
| Target control | `focus` brings a tab forward (`--no-window` leaves window focus alone); `use <target>` pins the current agent's default target; `--tab @latest` means the most recently opened / connected tab (`@1` is still list row 1) |
| Target identity | Single-target page responses carry top-level `resolved_target` / `browser` / `profile` / `warnings` / `hint`; background tabs add `tab_hidden` / `timers_throttled`, and the CLI prints "→ operating on: …" to stderr |
| Environment and extensions | `env` aggregates browser / Profile / extension / tab digest / persistent injections / warnings, with the default target and active tab first; `extensions` lists installed extensions (`management` is optional and must be granted by the user) |
| Element measurement | `measure --selector` returns rect / viewport / computed style / visible / `shadow_path`, supports `host >>> inner`, rejects `@e`, and no longer wraps a successful result in `js_return` |
| User scripts | `userscript <file-or-url>` parses `==UserScript==` and opens the install page; `inject --userscript` injects a minimal GM shim plus `--cleanup-selector` / `--cleanup-file` for old hosts |
| Screenshot waits | `screenshot --settle-ms` / `--wait-selector` / `--network-idle[=ms]` / `--require-cdp`; results carry `tab_visible` / `captured_tab` / `clip_box` / `degraded` / `warning` / `warnings` |
| tabs filters | `tabs --url-contains` / `--title-contains` / `--filter` / `--format json\|jsonl\|tsv`; large lists add `suggest` |
| console history | `console history` retrieves load-time error / unhandledrejection (`--settle-ms` / `--types` / `--clear`); `console tail` polls, it does not start capture |
| job progress | `job tail <id>` emits JSONL snapshots (`elapsed_ms` / `last_heartbeat` / `stalled`); `job result <exec_id>` retrieves a late exec result |
| Shadow DOM | `find` / `snapshot` / `read` recurse into open shadow roots; results carry `scope:"shadow"`, `host_selector` and `shadow_path`, and selectors are written as `host >>> relative-path`. Closed shadow roots are unreachable and reported via `shadow_closed_unreachable` |
| PDF pagination | Image-based PDFs are sliced into pages at 14200pt and normalised by `dpr` first, so HiDPI pages are no longer doubled; the response carries `pages` / `width_pt` / `height_pt` |
| `--max-height` | Now applies to the CDP path as well as the stitched fallback; overflow sets `truncated:true` |
| Download filename | `download --filename` keeps only the basename and rejects `..`, separators and Windows reserved names; the response reports `filename_sanitized` |
| Download cancel | `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT=1` cancels on timeout, `ATB_DOWNLOAD_CANCEL_ERASE=1` also erases the record |
| `broadcast` | Paths inside `--args` are made absolute; `--dry-run` produces a local plan |
| `scrape` | Transport failures become `outcome_unknown` (exit code 4) and are not retried; `--concurrency` / `--batch` accept 1..=32 |
| `tabs --limit 0` | Means "no truncation"; `tabs_count` stays the original total |
| `logs` | Returns JSON: `{ok, lines[], path, truncated}` |
| WS trust tiers | `ATB_WS_TRUST_MODE=compat\|whitelist\|token\|both`, default `compat`; use `both` on shared machines |
| Extension UI | Popup with Status / Data / Settings tabs plus a management page (theme, popup width 340 / 420 / 560, density, badge toggle, token and trust tier, cookies, diagnostics); settings live in `storage.local` and stay in sync between both entries; opening `popup.html` in a tab switches to a 780px page layout |

## Common workflows

### Read a page

```bash
atb tabs --ids
atb read --tab <tabId> --max-chars 8000
atb read --include-hidden          # include display:none content
atb scan --fast                    # compact text only, fast on large pages
```

Check top-level `resolved_target` before trusting a read: it tells you which tab and which browser / Profile the command actually hit. If `warnings` contains `tab_hidden` / `timers_throttled`, the target is in the background; rAF, IntersectionObserver and timers are paused or throttled, so run `atb focus --tab <id>` when lazy loading or animation matters. The CLI also prints "→ operating on: …" to stderr; `ATB_BROWSER_BANNER=off` disables it.

For `chrome://` / `chrome-extension://` / `about:` pages, `open` uses a 3-second wait budget by default (`ATB_OPEN_SPECIAL_WAIT_SEC`); pass `--wait-for none` when no page condition is needed. `chrome-extension://` can also open this tool's own management page.

### Fill a form and submit

```bash
atb snapshot --limit 200           # produces @e1 / @e2 handles
atb fill @e2 'Jane Doe' --dry-run  # validate arguments and target first
atb fill @e2 'Jane Doe'
atb click @e1
atb wait --selector '.result' --timeout 10
```

`snapshot` by default, and `click` / `fill` with plain CSS selectors, attach the debugger; `snapshot --js` plus `@e` handles use the in-page path and avoid the debug banner.

Do not use a fixed `sleep` to wait for a framework render: `atb wait --dom-stable` waits for 500ms of DOM silence (`ATB_WAIT_DOM_STABLE_MS`), and `atb wait --network-idle` waits for in-flight fetch / XHR to reach zero and settle; both accept an explicit window such as `--dom-stable 800`. When `wait --js` times out it returns the last five evaluations as `last_values`, so you can see how close the condition was.

### Testing user scripts (Tampermonkey-compatible)

Use this when a frontend script does not take effect on the real site, or when you need to reproduce a `GM_*` environment.

```bash
# 1. Open the target and bring it forward; background tabs throttle timers and lazy loading
atb open https://example.com/app --background
atb focus --tab @latest

# 2. Parse the ==UserScript== metadata first: @name / @match / @grant
atb userscript ./fix.user.js --parse-only

# 3. Inject with the GM shim and clean up old hosts
atb inject --userscript --file ./fix.user.js --gm-namespace fix-demo \
  --cleanup-selector '#legacy-host' --cleanup-selector '.old-panel'

# 4. Assert the effect and capture a screenshot
atb exec 'return {host: !!document.querySelector("#fix-host"), flag: window.__FIX__ === true}'
atb screenshot --selector '#fix-host' --out fix-host.png

# 5. Only use --persist when the script must survive navigation; default TTL is 2 hours
atb inject --userscript --file ./fix.user.js --persist fix-demo --match 'https://example.com/*' \
  --cleanup-selector '#fix-host'
atb inject --list
atb inject --remove fix-demo
```

- `inject --userscript` injects a minimal GM shim: `GM_getValue` / `GM_setValue` / `GM_deleteValue` / `GM_listValues` / `GM_registerMenuCommand` / `GM_log` / `GM_addStyle` / `GM_info` / `unsafeWindow`. `GM_xmlhttpRequest` is not supported and throws; `@require` is not loaded automatically.
- `inject` returns `executed` / `returned` / `threw` / `duration_ms` / `cleanup`. A throwing script sets top-level `ok:false` + `error_code:"inject_script_threw"`, but `executed:true` means side effects may already have happened.
- `--persist` expires after 7200 seconds (2 hours) by default; the extension's `persistTtlSec` setting participates in that default and `--ttl` overrides it. Permanent residency requires both `--persist-forever --i-know-this-is-long-lived`; `--list` reports `expires_in_sec`, `warning` and the total count.
- If the page already has a Tampermonkey-installed version, both copies inject their own host. Use `--cleanup-selector` to remove the old nodes, or ask the user to disable the installed copy temporarily; see [troubleshooting.md](./skills/atb/references/troubleshooting.md).
- `userscript <file-or-url>` only parses metadata and opens the install page: local files open as `file://` and are handled by Tampermonkey / Violentmonkey; `--parse-only` works only on local files and does not open a page, so download a URL first.

### Batch scraping

```bash
atb scrape --urls urls.txt --out out.jsonl
atb scrape --urls urls.txt --js-file grab.js --out out.jsonl --batch 6 --concurrency 6 --retry 2
```

`--urls` takes a file (one per line) or a comma-separated list; the default script collects `{title, url, text}`. Everything is orchestrated CLI-side on top of the daemon's open / exec / close.

Use `job tail` for progress and `job result` to retrieve a timed-out exec:

```bash
atb --async scan --text-only
atb job tail <jobId>                  # JSONL snapshots: elapsed_ms / last_heartbeat / stalled
atb job result <exec_id> --wait 60    # exec_id comes from the timeout error
```

`job tail` emits one JSONL line per state change; terminal exits are `0` / `1` / `130`, and a tracking timeout is `3`. `job result` returns `exec_result_not_found` when nothing appears inside the wait window (wait timeout exits `3`); late results are kept for 3600 seconds by default (`ATB_EXEC_RESULT_TTL_SEC`).

`exec` supports `--file a.js --file b.js` (concatenated in order), `--file -` for stdin, and `--max-chars` (visible alias of `--max-bytes`) for truncating the return value. Local JS balance checking defaults to `strict` (`ATB_JS_CHECK=off|warn|strict`); use `--no-preflight` when it is a false positive. Unreadable or oversized script files report the absolute path, size and `invalid_encoding` / `script_file_too_large`.

### Multiple tabs

```bash
atb open https://a.com https://b.com --background
atb read --tab all --max-chars 2000
atb exec --tab @1-3 'return location.href'
atb focus --tab @2
atb tabs --url-contains example --format tsv
atb screenshot --tab all --out 'shots/{index}-{tab}.png'
```

`tabs --format` accepts `json` (default) / `jsonl` (one tab per line) / `tsv` (`tab_id`, title, URL); `--url-contains` / `--title-contains` / `--filter` are applied daemon-side. Large lists add `suggest`, pointing at `--fields` / `--limit` or a narrower filter.

When a screenshot needs a settled render, use wait flags instead of a fixed `sleep`:

```bash
atb screenshot --wait-selector '#chart' --network-idle --settle-ms 200 --out chart.png
atb screenshot --full-page --require-cdp --out full.png
```

Bare `--network-idle` waits for in-flight fetch / XHR to reach zero and stay quiet for 500ms (`ATB_SCREENSHOT_NETWORK_IDLE_MS`); `--network-idle 800` overrides the window. `--settle-ms` defaults to `ATB_SCREENSHOT_SETTLE_MS` (0). `--require-cdp` fails with `cdp_required` instead of silently falling back to `captureVisibleTab` or stitching. The result's `tab_visible` / `captured_tab` tell you which tab was actually captured, `clip_box` echoes the CSS-px clip, and `degraded` / `warning` / `warnings` mark fallback channels.

Full-page screenshots using `captureBeyondViewport` can still repeat `position:fixed` / `sticky` elements at the bottom of the page; this is a browser rendering limit, not a clipping bug. For a clean long image, temporarily switch them to `position:absolute` (via `inject`) or use the stitched channel and accept repeated fixed elements per slice.

#### When the page blocks debugger injection (automatic fallback)

Chrome refuses `chrome.debugger.attach` on pages that embed an iframe from another extension
(common with assistant/translator extensions). These commands now degrade instead of failing:

| Command | Fallback channel | Marker in the response |
| --- | --- | --- |
| `screenshot` | visible area via `chrome.tabs.captureVisibleTab` (no debugger) | `via:"captureVisibleTab"` |
| `screenshot --full-page` | scroll slice by slice, stitch with OffscreenCanvas | `via:"captureFullPageStitched"`, `stitched:true`, `slices`, `truncated` |
| `save-pdf` | stitched long image wrapped into a single-page PDF (text not selectable) | `via:"stitched-image-pdf"`, `text_selectable:false` |
| `mouse-click` | synthetic mouse events dispatched in the page | `synthetic:true`, `trusted:false` |

`--max-height` caps the long-image height in device pixels: when omitted the CLI does not truncate, while the extension's stitched channel defaults to 20000; overflow is truncated and reported as `truncated:true`. Inherent limits of stitching: `position:fixed/sticky` elements repeat on every slice, and the PDF is image-only. Disable the offending extension temporarily if you need trusted input events or a searchable PDF; add `--require-cdp` when you need a deterministic channel, in which case CDP being unavailable fails instead of silently degrading.

### Multiple agents

```bash
atb --agent reviewer open https://a.com
ATB_AGENT=reviewer atb read
atb cleanup --dry-run      # preview what would be closed
atb cleanup                # close only tabs this agent opened
```

### Challenge pages

```bash
atb open https://example.com                 # --challenge auto by default: solve actively, then wait
atb challenge status                         # challenge state, widget position, solve verdict
atb challenge solve --timeout 120            # run the active solver again manually
atb challenge widget                         # solver failed: get widget coordinates
atb challenge click --at 320,410             # the agent clicks it itself, no human needed
atb challenge focus                          # only for image/SMS checks that need a human
```

`open` and `challenge solve` do not just wait: they locate the widget, click it with trusted mouse events that include a movement path, then wait for the token or navigation. If the solver fails, the agent can fall back to `widget` + `click --at`; `challenge wait` is the passive path and `focus` is the last resort for a human. Details in [docs/ANTI_DETECTION_AND_CHALLENGE.md](./docs/ANTI_DETECTION_AND_CHALLENGE.md).

## Command reference

There are **61** top-level commands; the list below covers common scenarios. `atb schema --json` is authoritative for the full set.

```bash
# Target and environment
atb focus --tab @latest                    # bring the most recent tab forward
atb use @latest                            # pin it as this agent's default target
atb env                                    # browser / Profile / extension / tabs / injections
atb extensions                             # installed extensions (management permission required)
atb measure --selector 'my-app >>> .card' --styles display,position,width,height
atb userscript ./fix.user.js --parse-only  # parse ==UserScript== only

# Reading
atb tabs --ids                            # compact list for target picking
atb tabs --url-contains github --format tsv
atb read --max-chars 8000 --links
atb snapshot --limit 200                  # element handles @e1 / @e2
atb find --text 'Sign in' --limit 5

# Actions
atb open https://example.com --background
atb click @e1
atb fill @e2 'text'
atb select '#city' --value sh
atb upload 'input[type=file]' ./a.pdf
atb wait --selector '.result' --timeout 10
atb close --tab <tabId>

# Scripts and batches
atb exec 'return document.title'
atb exec --file pre.js --file main.js     # concatenate in order; --file - reads stdin
atb inject --userscript --file fix.user.js --cleanup-selector '#old-host'
atb run steps.json                        # many steps in one process
atb batch steps.json --concurrency 8
atb scrape --urls urls.txt --out out.jsonl
atb --async scan --text-only              # returns a job_id immediately
atb job tail <jobId>                      # JSONL progress snapshots
atb job wait <jobId> --timeout 300
atb job result <exec_id> --wait 60        # retrieve a late result from a timed-out exec

# Network and debugging
atb net fetch https://api.example.com/me
atb net hook --capture-bodies
atb net mock /api/user --status 200 --body '{"id":1}'
atb console start
atb console history --types error,rejection   # load-time errors in one shot
atb console tail --duration 10                # poll-follow; does not start capture
atb downloads --limit 10

# Challenges and traces
atb challenge status
atb stealth audit
atb stealth scrub

# Operations
atb status
atb env                                   # full runtime environment in one call
atb extensions                            # installed extensions
atb doctor --fix
atb capabilities
atb schema --json                         # argument schema for top-level commands (use --help for subcommands)
atb logs --tail 100
```

`atb schema --json` is authoritative for parameters; the domain tables live in [skills/atb/references/commands.md](./skills/atb/references/commands.md).

### Exit codes

| Code | Meaning | What to do |
| --- | --- | --- |
| `0` | Success | — |
| `1` | Execution failed | Read `error_code`; retry after `retry_after_ms` when retryable |
| `2` | Command is wrong | Fix selector / JS / arguments / target; do not retry |
| `3` | `job wait` / `job tail` / `job result --wait` / `download-wait` timed out | Raise `--timeout`, or query again later; a plain `wait` timeout is `1` |
| `4` | Outcome unknown | The action may have run; resend with the same `--idempotency-key` |
| `130` | Job cancelled | `job wait` observed `cancelled` |

Error codes added in 0.8.0 (all carry a `hint`):

| error_code | Meaning | What to do |
| --- | --- | --- |
| `focus_failed` | Could not bring the tab forward | Confirm the tab with `tabs --ids`; reload the extension if it is outdated |
| `permission_required` | `extensions` lacks `management` | Grant it at extension settings "About → Advanced permissions", then retry |
| `daemon_outdated` | The resident daemon is an old binary without the route | `atb restart`, then retry |
| `syntax_error` | Local JS syntax check failed | Fix the reported line/column; if it is a false positive use `--no-preflight` or `ATB_JS_CHECK=off` |
| `inject_script_threw` | The script ran but threw | Read `executed` / `threw`; this is a script error, not a channel failure |
| `unsupported_selector` | Unsupported selector form (for example `@e` passed to `measure`) | Use `find` for a real CSS selector, or `host >>> inner` for shadow DOM |
| `exec_result_not_found` | The late exec result is missing or expired | Check `exec_id`; raise `ATB_EXEC_RESULT_TTL_SEC` for long tasks or switch to `--async` + `job wait` |
| `cdp_required` | `--require-cdp` was set but CDP is unavailable | Drop the flag, or disable the extension blocking the debugger |
| `invalid_encoding` | The script file is not UTF-8 | Convert it to UTF-8, or pass it via `--file -` on stdin |
| `script_file_too_large` | The script file exceeds the limit | Split it across multiple `--file` flags, or raise `ATB_SCRIPT_FILE_MAX_MB` |

## Versions and compatibility

| Item | Value |
| --- | --- |
| CLI / daemon | `0.8.0` |
| CLI ↔ daemon API protocol | `5` |
| Extension message protocol | `4` |
| Extension file version | `5.5.0` |
| Minimum Chrome | `120` |

- Current version: `0.8.0`
- Companion extension: `5.5.0` (display name `ATB`)

```bash
atb status
# compatibility: { ok, cli_protocol, daemon_protocol,
#                  extension_protocol_expected, extension_protocol_connected, fixes[] }
```

| Symptom | What to do |
| --- | --- |
| `compatibility.ok:false` with different protocol numbers | `atb restart`; if it persists, end the process holding `18767` |
| `error_code:"extension_outdated"` | Replace the extension directory with `assets/atb_bridge`, then click "Reload" on the extension page |
| `upgrade_recommended:true` in `status` | Core commands still work; update the extension following `next_steps` |
| `stealth` returns `extension_ok:false` | Extension older than 5.0.0, or files replaced without a reload; follow `fixes`, then re-check |

Upgrade note: a new CLI refuses to talk to an old daemon on a protocol mismatch and prints the fix steps. The config directory stays `~/.atb` on purpose — moving it would split old and new daemons across separate ports.

## Operations

```bash
atb status                       # ok follows healthy; non-zero exit when unhealthy
atb doctor                       # diagnose ports, token, daemon version
atb doctor --fix                 # auto-repair; --force is required to kill processes
atb logs --tail 100
atb restart                      # restart the daemon after protocol or config changes
atb stop                         # stop the daemon; frees 18765, and the extension keeps reconnecting
atb set-extension-port 18766     # change the extension port, writes config.json and restarts
atb install-skill --dry-run      # preview where the skill will land
atb install-skill                # real dir ~/.agents/skills/atb; symlinks into the five agent dirs
```

`atb env` aggregates `browser` / `profile` / `extension` / `tabs` / `injections` / `warnings` in one call: `tabs.shown` starts with the default target and the active tab, each carrying `is_default`; `warnings` flags an outdated extension, a background default target, persistent injection counts and similar issues. `atb extensions` depends on the optional `management` permission and returns `permission_required` until you grant it under extension settings "About → Advanced permissions".

`18767` is the CLI HTTP API port: fixed, not configurable, and never usable as the extension port. `ATB_EXIT0=1` restores the old always-zero `status` exit code — set it only if a script depends on that.

Upgrading the CLI: `npm i -g @g1728578755/atb@latest`, then update the extension files as described in "Load the extension". Full history in [CHANGELOG.md](./CHANGELOG.md); an AI-driven upgrade flow in [AI_INSTALL.md](./AI_INSTALL.md).

## Environment variables

Every threshold, tier and switch can be tuned with `ATB_*`; the legacy `AGENT_BROWSER_CLI_*` prefix still works and the new name wins. The 70 user-facing variables are grouped below; the 9 test/CI-only ones are listed separately at the end.

### Identity and location

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_AGENT` | terminal session id → `default` | Caller identity; decides the default target and `cleanup` scope |
| `ATB_HOME` | `~/.atb` | Config directory; affects config and tokens only, not `~` expansion |
| `ATB_BIN` | auto-resolved | Point at a self-built atb binary |
| `ATB_EXIT0` | off | Restore the old always-zero `status` exit code; must be `1` / `true` |
| `ATB_NO_DEBUGGER` | off | Disable `chrome.debugger` attach entirely |
| `ATB_BROWSER_BANNER` | `once` | CLI stderr "→ operating on: …" banner: `off` / `once` / `always` |

### Result semantics and timeouts

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_RESULT_STRICT` | `warn` | Strictness of composite commands: `off` / `warn` / `strict` |
| `ATB_HIDDEN_TAB_WARNING` | `true` | Whether background targets add `tab_hidden` / `timers_throttled` warnings and a `focus` hint |
| `ATB_OPEN_SPECIAL_WAIT_SEC` | 3 | `open --wait-for` budget for `chrome://` / `chrome-extension://` / `about:` pages (0.5–60s) |
| `ATB_WAIT_DOM_STABLE_MS` | 500 | DOM silence window for `wait --dom-stable` (50–10000ms) |
| `ATB_WAIT_MAX_TOTAL_SEC` | 600 | Total budget for one `wait` / `--wait-js` command (1–3600s) |
| `ATB_WAIT_MIN_INTERVAL_MS` | 50 | Polling floor, prevents `interval=0` busy looping |
| `ATB_HTTP_SLACK_SEC` | 10 | Extra HTTP timeout margin added on top of the business timeout |
| `ATB_CHALLENGE_MAX_IFRAMES` | 8 | Iframes processed per challenge probe (1–64) |
| `ATB_CHALLENGE_CLICK_TIMEOUT_SEC` | 8 | Hard cap for the whole coordinate-click chain |
| `ATB_DIAG_DEPTH` | 3 | Root-cause entries kept in diagnostics (0–20) |

### Files and artifacts

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_IO_OVERWRITE` | `backup` | When `--out` exists: `backup` / `force` / `ask` / `append` |
| `ATB_IO_SANITIZE_FILENAME` | `strict` | Download filename sanitising; `compat` only warns |
| `ATB_IO_MAX_OUTPUT_MB` | 50 | Per-file / buffer limit (1–512MB) |
| `ATB_SCRIPT_FILE_MAX_MB` | 32 | Per-file limit for `exec --file` / `inject --file` (1–256MB) |
| `ATB_SCREENSHOT_SETTLE_MS` | 0 | Extra wait before `screenshot` when `--settle-ms` is omitted (0–10000ms) |
| `ATB_SCREENSHOT_NETWORK_IDLE_MS` | 500 | Silence window for `screenshot --network-idle` (50–10000ms) |
| `ATB_CAPTURE_MAX_PIXELS` | 80000000 | Pixel budget for stitched screenshots / image PDFs (1M–500M) |

### Privacy and idempotency

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_IDEMPOTENCY_PERSIST` | `redacted` | Idempotency persistence: `redacted` (metadata only) / `full` (with body) / `off` |
| `ATB_IDEMPOTENCY_FILE_MAX_MB` | 32 | Runtime cap for `idempotency.jsonl`, compacted in the background (1–512MB) |

### Sessions and multi-agent

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_SESSION_ISOLATION` | `legacy` | In `strict` mode a named agent without a target fails with `target_lost` |
| `ATB_MULTI_SCOPE` | `agent` | Default ownership scope for `--tab all`; `all` crosses agents |
| `ATB_PREFERRED_MISS_LIMIT` | 3 | Consecutive misses before dropping the preferred default target |
| `ATB_PREFERRED_MISS_TTL_SEC` | 120 | How long a preferred target may keep missing (1–86400s) |

### Scripts, injections and jobs

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_JS_CHECK` | `strict` | Local JS balance check: `off` / `warn` / `strict`; `--no-preflight` skips one command |
| `ATB_EXEC_RESULT_TTL_SEC` | 3600 | Retention for late exec results (60–86400s) |
| `ATB_INJECT_PERSIST_DEFAULT_TTL_SEC` | 7200 | Default lifetime for `inject --persist` without `--ttl` (60–2592000s) |
| `ATB_INJECT_PERSIST_MAX_TTL_SEC` | 2592000 | Maximum accepted `inject --ttl` (60–31536000s) |
| `ATB_CONSOLE_HISTORY_SETTLE_MS` | 300 | Sampling window after `console history` attaches (50–5000ms) |
| `ATB_JOB_STALL_SEC` | 60 | Heartbeat timeout for `stalled` in `job list` / `job tail` (5–3600s) |
| `ATB_TABS_LARGE_WARNING` | 50 | `tabs` adds `suggest` above this count (10–500) |

### Downloads

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_DOWNLOAD_AUTO_CLAIM` | `true` | Whether `download-wait` without `--id` auto-claims a recent download |
| `ATB_DOWNLOAD_CLAIM_TTL_SEC` | 600 | Auto-claim "recently completed" window (30–900s) |
| `ATB_DOWNLOAD_REGISTRY_DEPTH` | 8 | Download ids remembered per agent (1–64) |
| `ATB_DOWNLOAD_WAIT_LIMIT` | 200 | Rows queried per `download-wait` round (1–500) |
| `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT` | off | Cancel the download when waiting times out |
| `ATB_DOWNLOAD_CANCEL_ERASE` | off | Erase the download record after cancelling |

### Security and network boundaries

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_WS_TRUST_MODE` | `compat` | WS trust tier: `compat` / `whitelist` / `token` / `both` |
| `ATB_WS_OUTBOUND_MAX` | 512 | Downlink control channel capacity (8–65536) |
| `ATB_FILE_HOSTS` | empty | Allowed `file://host/...` hosts, comma separated (localhost only by default) |
| `ATB_HTTP_NO_PROXY` | `true` | Skip the system proxy for loopback addresses |

### Network and monitoring

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_NET_LOG_ITEM_CHARS` | 8192 | Per-item url/body/headers character limit (256–1M) |
| `ATB_NET_RULE_MAX_BYTES` | 2MiB | Per-rule body byte limit (1KiB–16MiB) |
| `ATB_NET_RULE_MAX_RULES` | 64 | Rules per profile (1–500) |
| `ATB_NET_MAX_ENTRIES` | 5000 | Network buffer limit (100–100000) |
| `ATB_CONSOLE_MAX_ENTRIES` | 5000 | Console buffer limit (100–100000) |
| `ATB_MONITOR_SAMPLES` | 50 | Samples kept by `--monitor` (1–500) |

### Startup and locks

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_LOCK_RETRIES` | 40 | Lock-file retry count |
| `ATB_LOCK_RETRY_MS` | 25 | Lock retry interval (ms) |
| `ATB_PROC_CMD_TIMEOUT_MS` | 5000 | Timeout for process enumeration commands |
| `ATB_LAUNCH_SETTLE_MS` | 800 | Wait for the window to settle after `launch` |
| `ATB_LAUNCH_CLOSE_WAIT_MS` | 3000 | Max wait for a graceful browser close |
| `ATB_LAUNCH_KILL_WAIT_MS` | 8000 | Max wait after a forced kill |
| `ATB_LAUNCH_POLL_MS` | 500 | Process polling interval during launch |
| `ATB_BROWSER_EXE` | auto-detected | Explicit browser executable path |

### Challenges and HTML

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_HTML_KEEP_SCRIPTS` | off | Keep `<script>` during HTML optimisation (debugging) |
| `ATB_CHALLENGE_SRCDOC_MIN_AREA` | 2000 | Minimum area (px²) for srcdoc / about: iframes to be considered |
| `ATB_CHALLENGE_FOCUS_ON_NEED_HUMAN` | `true` | Bring the tab forward when a human is needed |
| `ATB_EXTENSION_ID` | auto-detected | Identify the tool's own iframe when several extensions coexist |

### Idle and concurrency

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATB_IDLE_TTL` | 300 | Idle seconds before exit when no extension is connected |
| `ATB_MAX_RUNNING_JOBS` | 32 | Concurrent job cap; running jobs are aborted before hard eviction |
| `ATB_REQUIRE_TAB` | off | Write commands without `--tab` exit 2 instead of using the default tab |
| `ATB_FORCE_ASSUME_ATB` | `strict` | Identity rule for `restart --force` / `doctor --fix`; `loose` relaxes it |
| `ATB_ALLOW_RELATIVE_OUT` | off | Keep the old relative-path behaviour for `broadcast` output |
| `ATB_BROWSERS_SNAPSHOT_TTL_SEC` | 5 | Short snapshot TTL for `browsers`; `force_refresh` bypasses it |

### Test / CI only (not for daily use)

| Variable | Purpose |
| --- | --- |
| `ATB_E2E` / `ATB_E2E_BIN` / `ATB_E2E_AGENT` | Scenario selection, binary path and agent name for the e2e scripts |
| `ATB_EXT_DIR` | Extension directory override (deploy/test scripts only) |
| `ATB_PACKAGE_DIR` | npm package directory override |
| `ATB_PLATFORM_OVERRIDE` | Force a platform key in `npm/bin/atb.js` |
| `ATB_SKIP_POSTINSTALL_CHECK` | Skip the postinstall binary probe |
| `ATB_ALLOW_VERSION_MISMATCH` | Allow a platform package version mismatch (warning only) |
| `ATB_LOCK_ALLOW_MISSING_INTEGRITY` | Allow a missing `integrity` in the lock check (release window) |

## Security and privacy

- Traffic between the CLI, daemon and extension stays on `127.0.0.1`; no telemetry, no remote code, no ad SDK. The pages you drive still talk to their own sites.
- `trust-extension` is one-way: it constrains who may connect to the daemon, but the extension never verifies that the daemon it talks to is genuine.
- The daemon exits after 300 idle seconds only when no extension is connected; `atb stop` alone frees 18765 while the extension keeps reconnecting, so disable the extension or close the browser to truly close the window.
- The extension requests `cookies`, `tabs`, `tabGroups`, `activeTab`, `debugger`, `scripting`, `alarms`, `storage`, `declarativeNetRequest`, `webNavigation` and `downloads` (the manifest is authoritative). `management` is an **optional permission**: it is enabled only after the user clicks "About → Advanced permissions" in extension settings, and powers `atb extensions`; without it the command returns `permission_required` instead of quietly listing only this extension. Page data is read only when you or the CLI issue a command; `blockingExtensions` now reports only the blocking extension's id / origin, not its name.
- WS trust defaults to `compat`: any local process can connect to 18765. On shared machines set a whitelist plus token and switch `ATB_WS_TRUST_MODE` to `both`, then `atb restart` — configuring without switching has no effect. `security.warning` in `atb status` reports the current tier.
- Idempotency defaults to `ATB_IDEMPOTENCY_PERSIST=redacted` (key / scope / time / status, no response body); `daemon.log` is 0600 in a 0700 directory, and `ws-trust.json` gets the same treatment as `api-token`.
- `challenge` interacts with the page's verification widget (click and wait).

Report vulnerabilities per [SECURITY.md](./SECURITY.md); the local data inventory and deletion steps are in [PRIVACY.md](./PRIVACY.md). Both documents promise only what the code can guarantee and mark everything else as a boundary.

## Platforms and limits

Supported: macOS arm64 / x64, Linux glibc x64 / arm64, Windows x64, WSL 2 (use the Linux package), Chrome and Edge 120 or newer.

What it explicitly cannot do:

- No prebuilt packages for Windows ARM64 (`win32-arm64`) or Alpine / musl — build from source.
- The debug banner is Chrome's own UI and cannot be removed at runtime; the only switch is a launch flag, and applying it requires restarting the browser via `atb launch --restart`.
- Firefox / Safari are not supported; it needs a Chromium-based browser plus the extension.
- Fingerprint spoofing (UA / canvas / WebGL / hardware values) is not yet implemented and is listed as future work; `stealth` currently only removes what this tool wrote into the page.
- JS challenges and clickable Turnstile are handled automatically; image selection, sliders, and SMS/email checks are not yet implemented (future work).
- Background tabs pause `requestAnimationFrame` / `IntersectionObserver` and throttle timers; lazy loading, animation and visibility checks need `atb focus` first, and the `tab_hidden` / `timers_throttled` warnings point at it.
- Full-page screenshots using CDP `captureBeyondViewport` can still repeat `position:fixed/sticky` elements at the bottom; the stitched channel repeats them on every slice. This is a browser rendering limit, not a clipping bug.
- When the extension bridge is down, no tab can be driven.

## Documentation map

| Reader | Document | Contents |
| --- | --- | --- |
| Letting an AI install it | [AI_INSTALL.md](./AI_INSTALL.md) | Copyable prompt, step-by-step flow, failure branches, uninstall |
| Writing an agent | [skills/atb/SKILL.md](./skills/atb/SKILL.md) | Standard loop, command selection by intent, failure handling |
| Looking up flags | [commands.md](./skills/atb/references/commands.md) | Per-domain command and parameter tables |
| Wanting recipes | [recipes.md](./skills/atb/references/recipes.md) | Copy-paste scenarios |
| Debugging a failure | [troubleshooting.md](./skills/atb/references/troubleshooting.md) | Symptom → diagnosis → fix |
| Going deeper | [docs/BEST_PRACTICES.md](./docs/BEST_PRACTICES.md) | Selectors, waiting, token budgets |
| Long-running work | [docs/ASYNC_BACKGROUND.md](./docs/ASYNC_BACKGROUND.md) | Async jobs, background tabs, limits |
| Detection and challenges | [docs/ANTI_DETECTION_AND_CHALLENGE.md](./docs/ANTI_DETECTION_AND_CHALLENGE.md) | Trace audit, scrubbing, challenge handling |
| Edge load failures | [docs/EDGE_TROUBLESHOOTING.md](./docs/EDGE_TROUBLESHOOTING.md) | Extension load errors, one by one |
| Feature status | [docs/FEATURE_STATUS.md](./docs/FEATURE_STATUS.md) | Shipped / next / performance and boundaries |
| Release history | [CHANGELOG.md](./CHANGELOG.md) | Change summaries since 0.4.0 |
| Historical record | [docs/CHANGELOG_ARCHIVE.md](./docs/CHANGELOG_ARCHIVE.md) | 0.3.7 and earlier, plus pre-refactor detail |
| Contributing | [CONTRIBUTING.md](./CONTRIBUTING.md) | Layers, test matrix, contracts you cannot break |
| Community norms | [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md) | Expected behaviour, reporting channel |
| Security / privacy / sources | [SECURITY.md](./SECURITY.md) · [PRIVACY.md](./PRIVACY.md) · [SOURCES.md](./SOURCES.md) | Threat model, local data, upstream and dependencies |

## Contributing

Read [CONTRIBUTING.md](./CONTRIBUTING.md) before changing code: exit codes, `error_code` values, JSON fields and both protocols are external contracts. Before submitting, run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `node --test tests/*.test.mjs`.

Documentation changes ship in the same PR; the Chinese and English READMEs must stay in sync, and version strings are written by `npm run sync-version`.

## Acknowledgements

The browser control capability was extracted and adapted from the Web toolchain in [GenericAgent](https://github.com/lsdefine/GenericAgent), including `TMWebDriver`, `simphtml`, and the `atb_bridge` extension. Upstream sources and dependency licences are listed in [SOURCES.md](./SOURCES.md).

## License

MIT License, Copyright (c) 2026 1728578755 / sleepinginsummer / lsdefine. See [LICENSE](./LICENSE).
