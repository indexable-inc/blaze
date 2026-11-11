# harness on iOS

The same app as the Mac's: its core, scenes and renderer are shared, and `src/ios.rs` is the UIKit shell in place of
AppKit's `src/window.rs`. This folder holds what a bundle needs besides the binary.

In the simulator (it runs on the Mac, so it reaches the Mac's daemon socket through `HARNESS_HOME`):

```sh
# from crates/harness; libghostty's zig build needs release mode for iOS
LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast cargo build --release --target aarch64-apple-ios-sim -p harness-native
mkdir -p Harness.app && cp target/aarch64-apple-ios-sim/release/harness-native harness-native/ios/Info.plist Harness.app/
xcrun simctl boot "iPhone 17 Pro" && xcrun simctl install booted Harness.app
# a home whose path is short enough for a socket address, holding a link to the daemon's socket
mkdir -p /tmp/hh && ln -sf ~/.harness/harnessd.sock /tmp/hh/harnessd.sock
SIMCTL_CHILD_HARNESS_HOME=/tmp/hh xcrun simctl launch booted dev.harness.native
```

Not yet: a device build needs signing with a development team and a way to reach the daemon over the network (it
listens on a Unix socket only); touch scrolling does not glide after the finger lifts; the Mac's menus, pasteboard,
browser pane and tuck-away have no counterpart here.
