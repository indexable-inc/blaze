# harness-browser

The harness's own browser: Chromium Embedded Framework (CEF), pinned, run by the daemon, drawn by the app. Agents drive
it over DevTools (the kernel's `googleSearch`, `gmail.*`, `withHiddenPage`, `browser.open`); the person sees each page
the agents use as a live card in the chat and uses it in a pane on the window's right. It replaces Dia (or any browser
started with `--remote-debugging-port`) as the kernel's browser. There is no fallback to Dia: when the host is not up,
every browser call fails naming it.

## The pin

| | |
| --- | --- |
| CEF | `156.0.15+gcf671c0+chromium-156.0.8078.80` (stable, 2026-10-31), Chrome bootstrap |
| Archive | `cef_binary_156.0.15+gcf671c0+chromium-156.0.8078.80_macosarm64_minimal.tar.bz2` from `https://cef-builds.spotifycdn.com/` |
| sha1 | `bcdd1877edb300b0fb91d8f71c0b14aae256cd0f` (what the CDN's `index.json` lists) |
| sha256 | `a27f1d856e84047657ed2a14caf355fb04480c4fe1963f04249a8a3184af1c30` (ours, taken on download 2026-11-06) |
| Rust | the `cef` crate `=156.3.0+156.0.15` (tauri-apps/cef-rs, generated bindings of this exact CEF), default features off; its `cef-dll-sys` patched to `vendor/cef-dll-sys` |

One place holds the pin: `scripts/cef.py` (`PIN`). `vendor/cef-dll-sys/build.rs` repeats the version only to find the
directory, and fails naming `cef.py` when it is not there.

## Fetch, build, bundle

    uv run --frozen crates/harness/scripts/cef.py '{"op": "fetch"}'      # once per pin, ~135 MB
    cd crates/harness && cargo build --release -p harness-browser -p harness -p harness-native
    uv run --frozen crates/harness/scripts/cef.py '{"op": "bundle", "profile": "release"}'

- `fetch` downloads the archive, checks both hashes, and extracts it to
  `~/Library/Caches/inc.indexable.harness/cef/<version>/` (the directory named for the archive is renamed `cef_macos_aarch64`
  inside, the layout `cef-dll-sys` reads; a `.verified` file records the hashes). Nothing is fetched again while it is
  there. No nix, no Homebrew, nothing installed on the host.
- `vendor/cef-dll-sys` is cef-rs's sys crate with its build script replaced: it never downloads and never runs cmake. It
  reads that directory (or `CEF_PATH`), compiles the one wrapper file cef-rs's C API needs on macOS
  (`libcef_dll/wrapper/libcef_dll_dylib.cc`, the stubs that call into the framework once it is loaded) with the `cc`
  crate at `CEF_API_VERSION` 15600, and links AppKit. A missing or unverified CEF is an error naming `cef.py fetch`.
- `bundle` makes `crates/harness/target/<profile>/Harness Browser.app`, the layout CEF requires on macOS:

```text
Harness Browser.app/Contents/
  Info.plist                                   inc.indexable.harness.browser, LSUIElement (no Dock icon)
  MacOS/Harness Browser                        the harness-browser binary
  Frameworks/Chromium Embedded Framework.framework     cloned (APFS clonefile) from the cache
  Frameworks/Harness Browser Helper.app                 Contents/MacOS/Harness Browser Helper     (the same binary)
  Frameworks/Harness Browser Helper (GPU).app           ... (GPU)
  Frameworks/Harness Browser Helper (Renderer).app      ... (Renderer)
  Frameworks/Harness Browser Helper (Plugin).app        ... (Plugin)
```

  Each helper has its own Info.plist (`inc.indexable.harness.browser.helper[.gpu|.renderer|.plugin]`), and every
  bundle is signed ad hoc, inside out (arm64 runs nothing unsigned). The daemon finds the bundle at `$HARNESS_BROWSER_APP`,
  else beside its own executable (`target/<profile>/Harness Browser.app`), and its error names both and `cef.py bundle`.

## Processes

The **daemon** owns the browser: the daemon outlives the app, and agents use the browser with no window open.

- `harness` (the daemon) starts the host on the first `browser` request (the kernel's first browser call, or the app at
  start) and answers `Reply::Browser { pid, sock, service }` once the host printed its ready line; a second request while
  it runs answers at once; a host that died is started again by the next request. The host gets `HARNESS_HOME` and
  `HARNESS_DAEMON_PID` and exits when that pid does (kqueue `EVFILT_PROC NOTE_EXIT`); the daemon kills it on shutdown.
- `Harness Browser` (the host) is CEF's browser process, its main thread CEF's UI thread. Every page is an off-screen
  view: a windowless browser (Alloy style, `shared_texture_enabled`), its frames IOSurfaces from Chromium's GPU process.
- The helpers are CEF's renderer, GPU and plugin processes, the same binary: with `--type=` it initialises the
  macOS sandbox (`libcef_sandbox.dylib`'s `cef_sandbox_initialize`, before the framework is loaded), loads the framework
  from `../../..` and runs `cef_execute_process`. Renderers run sandboxed; `no_sandbox` is never set.
- The **app** draws views and forwards input. It never runs Chromium.
- **No notifications.** The browser process runs with `--disable-notifications` and the `SystemNotifications` and
  `NativeNotifications` features off (handlers.rs), and the bundle has no `(Alerts)` helper: nothing reaches
  UNUserNotificationCenter, so macOS never asks for notification permission. A page's `Notification.requestPermission()`
  is denied.

State is per daemon, `$HARNESS_HOME/browser/` (0700): `profile/` (CEF's `cache_path`, under `root_cache_path` = the
directory: cookies, logins, history, Web Store extensions), `extensions/` (unpacked extensions, each loaded at start
with `--load-extension`), `host.sock`, `cef.log`.

## Kernel ⇄ host: the socket

`$HARNESS_HOME/browser/host.sock`, 0600; no TCP port at all (`remote_debugging_port` stays 0, so nothing on localhost is
reachable by other users or other programs). harness frames, CBOR messages `harness_proto::browser::{KernelUp,
KernelDown}`. CDP goes through the host:

- `Open { url }` makes a view and answers `Opened { view }` with its page target id, read by sending
  `Target.getTargetInfo` to the new browser over `CefBrowserHost::SendDevToolsMessage`.
- `Cdp { view, message }` is passed to that browser's `SendDevToolsMessage`; the host renumbers ids per connection (a
  view two clients drive never mixes their answers), and a `CefDevToolsMessageObserver` on each view sends its answers
  back to the connection that asked and its events to every connection attached to the view. A dropped connection's
  ids stay taken until answered or the view closes, so a late answer never reaches a later request that reused its
  id (`src/cdp_ids.rs`, and its DST in `src/cdp_ids/dst.rs`).
- So the kernel's CDP client is the same flat-session client as before, over a different transport, with the page's own
  domains (Page, Runtime, Network, Input, DOM, Emulation, Fetch...). The browser-level `Target.createTarget` is gone: a
  page made that way would be a Chrome-style native window, not an off-screen view. `browser().newPage()` and
  `withHiddenPage` call `Open` instead.

## App ⇄ host: Mach

The app asks the daemon for `browser` at start, looks up `service` (`harness-mach`), and sends `AppUp::Hello` carrying its
receive port. Frames:

1. `AppUp::Watch { view, on }` for each view something on screen shows; `AppUp::Resize` with the box it is drawn in.
2. The host keeps, per watched view, a **ring** of 3 IOSurfaces of its own (BGRA8, the view's pixel size), sent once as
   `AppDown::Ring` with one port per slot, again only when the size changes (a new generation).
3. On `OnAcceleratedPaint` (whose IOSurface is CEF's pool's, valid only during the call) the host blits it into a free
   slot with Metal, waits for that blit (well under a millisecond), and sends `AppDown::Frame { slot, seq }`. A select
   box's popup (`PET_POPUP`) is kept in a texture of its own and blitted over the view at its rect, so the app sees one
   picture.
4. The app wraps each slot's IOSurface in an `MTLTexture` once per ring (`newTextureWithDescriptor:iosurface:plane:`),
   draws the newest slot as the node's `Surface`, and says `AppUp::Latched { slot }`. The host writes only into a slot
   the app can neither be sampling nor come to sample: not the one it latched, not the newest picture, and not one an
   outstanding frame names (sent, and not superseded by a later latch: the app adopts frames in the order sent, so its
   latch of seq `s` frees every frame before `s`). When none is free the paint is dropped and the latch that frees one
   asks for a repaint; a frame the app's full queue refused frees its slot and repaints 16 ms later. The rule is the
   sans-IO core `src/slots.rs`, proven by its DST (a model of the app, FIFO queues, Full sends, resizes; safety at every
   write and the newest picture shown once still), with the earlier "neither latched nor newest" rule as a planted bug
   it catches: a frame in flight could name the slot it wrote.
5. A view nobody watches still renders (agents screenshot over CDP), at 10 fps, with no blits.

Input goes the other way, all in view points: `Pointer` (move, down, up, leave, wheel; buttons, click counts, CEF's
modifier bits), `Key` (the Mac key code, which the host maps to Windows' code as CEF wants, plus the characters; `Down`,
`Up`, then `Char` per typed unit), `Ime` (set marked text, commit, finish, cancel: the app is the NSTextInputClient and the
page gets a real composition), `Focus`, `Nav`. The host answers with `Cursor` (a shape name the app knows), `ImeRect`
(where the candidate window goes) and `Views` whenever a url, title or loading state changes.

## In the app

- **The primitive.** `harness-native-ui-browser-api` (components) and its prim in `harness-native-ui-prims/scene/` make a
  browser view a normal node: `ui.Browser` with `BrowserView { view }` lays out like any node, draws as a `Surface` keyed
  by the view (harness-native-render's Media holds each view's ring as textures, beside Shaders and Videos), hit-tests and
  takes the pointer, wheel and (with `BrowserFocus`) keys and IME, which `harness-native/src/app/browser.rs` forwards.
- **The card.** When a cell uses the browser, its step's shown value is `{ t: "browser", view, url, title }`
  (`mcp/src/shown.ts`); chat.rs's `value_rows` draws it as the genui `Browser` element in its compact form: a live picture
  of the page, its title and host under it. Agents can also draw one themselves: `<Browser view={page.view} />` or
  `<Browser src="https://…" />`. Pressing a card opens its view in the pane.
- **The pane.** `hn.WebPane`, beside the main card on the right: a toolbar (back, forward, reload, the URL field, the
  extensions button, close) over the view, sized by its own spring like the left sidebar's fold. Its state is data on one
  entity, `hn.web`: `WebPane { view, open, width }` and the tabs it lists; `act.web.*` actions and keys in 20-keys.flecs
  (cmd+shift+B shows or hides it).

## Extensions

Andrew wants extensions. What CEF 156 gives:

- The Chrome bootstrap runs Chrome's own extension system; the old Alloy `CefExtension` / `LoadExtension` API was removed
  in M128. An extension installed in the profile runs in every view of that profile, off-screen ones included: content
  scripts, background service workers, `declarativeNetRequest`, password managers' autofill scripts.
- What off-screen (Alloy-style) views lack is Chrome's toolbar: no extension action buttons, so no action popups, and the
  Web Store's install button needs a Chrome-style browser.

So:

- **Installing.** `browser.manage()` (kernel), the pane's extensions menu ("Manage extensions…", "Chrome Web Store…") or
  `KernelUp::Manage` opens the **manage window**: a Chrome-style CEF window (CEF Views, `CEF_RUNTIME_STYLE_CHROME`) of
  the host's own, at `chrome://extensions` or the Web Store, in the same profile. Installs there land in
  `profile/Default/Extensions` and work in every view. Unpacked extensions go in `$HARNESS_HOME/browser/extensions/<name>/`
  and are loaded with `--load-extension` (they need a host restart: `harness browser restart`, or the daemon's next start).
- **Action popups.** The pane's extensions menu lists `Extensions` (read from the profile's preferences and manifests);
  choosing one sends `AppUp::Action`, and the host opens `chrome-extension://<id>/<default_popup>` as a small view of its
  own (owner `popup:<view>`) that the app floats under the toolbar. Such a popup has no tab context of its own
  (`chrome.tabs.query({active: true})` finds no tab), so popups that act on "the current tab" may not work; their content
  scripts do. When one needs Chrome's toolbar, the manage window has it.

## Logged-in sessions and Gmail

The profile starts empty: Dia's cookies are Dia's. Andrew signs in to Google once in the pane (the profile keeps the
session). The host never touches the login Keychain: it runs with `--use-mock-keychain` (and os_crypt_async's
`UseKeychainKeyProvider` off), because a Keychain item would make macOS ask for the person's Keychain password after
every rebuild (a new ad hoc signature), with every navigation waiting on the answer. Chromium on macOS has no
file-backed cookie key, so the key is the mock's fixed one, and the profile is protected by `browser/` being 0700. Until then
`gmail.*` fails naming it: "Gmail is not signed in in the harness browser: open mail.google.com in the browser pane and
sign in". Google may refuse sign-in in embedded browsers; if it does, that error says so and the explicit path is:

- `gmail.via({ cdp: "http://localhost:9222" })` (and `HARNESS_GMAIL_CDP`) points the gmail helpers, and only them, at an
  external DevTools endpoint (Dia started with `--remote-debugging-port`). It is an explicit opt-in for the kernel's
  life, never chosen on its own when the harness browser fails.

## What is not done (yet)

- Drag and drop into a view, file choosers (`OnFileDialog` answers with an NSOpenPanel in the app), print, downloads UI
  (downloads land in `~/Downloads` with no prompt), permission prompts (camera, notifications: refused, named in
  `cef.log`).
- Fixtures for the new messages in `harness-proto/fixtures/` (the kernel's wire test round-trips only schema 1's).

## Implementation notes

- `src/main.rs` splits the helper (`--type=`: its own `cef_sandbox_initialize`, the framework at `../../..`,
  `cef_execute_process`) from the browser process (the framework at `../Frameworks`, the `app_class` NSApplication,
  `cef_initialize`, the message loop).
- `src/host.rs` owns all state on the UI thread. Other threads `post` a command: a crossbeam channel plus one CEF task
  per command. No CEF call is made while the host is borrowed, because CEF calls back synchronously.
- `src/cdp_ids.rs` and `src/keys.rs` are sans-IO cores with unit tests. `src/ring.rs` holds the Metal blits and the
  IOSurface lifetime rules.
- A view the kernel opens renders at scale 1 until the app sends `Resize`. An extension's action popup opens at
  400x600 points.
- `--load-extension` is passed together with `--disable-features=DisableLoadExtensionCommandLineSwitch`, which
  Chromium 137+ needs before it honours the switch.
