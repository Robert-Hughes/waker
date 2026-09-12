# Android GameActivity lifecycle and platform integration

Waker's Android host is an AndroidX `GameActivity`, driven directly through `android-activity`. Android deliberately does **not** use winit or eframe as its platform integration layer.

The shared application/UI code remains Rust + egui:

```text
WakerApp / egui UI
├── desktop
│   └── eframe → winit → wgpu
└── Android
    └── GameActivity → android-activity → egui → egui-wgpu → wgpu
```

The Java/Kotlin side is intentionally small. `WakerActivity` subclasses GameActivity without application logic, and the existing `LogOpener` helper exports a credential-sanitised log through MediaStore. Rendering, networking, UI state and lifecycle handling remain in Rust.

## Why Android does not use winit

This is an intentional response to two Android lifecycle failures captured on the target Android foldable on 11 September 2026.

The first failure occurred during unlock. Android detected a `CONFIG_UI_MODE` change and attempted to recreate Waker's Activity. The old Activity became logically resumed/visible while its real window remained hidden and surface-less, leaving the splash screen stuck above it.

The second failure showed that the problem was broader than configuration changes. After a real Activity finish/destroy, Android could retain the Waker process but a later launch could end up with an ActivityRecord and no usable native window.

The dependency behaviour explained both failures:

- `android-activity 0.6.1` treats an Activity lifetime explicitly: `MainEvent::Destroy` means the current `android_main()` invocation must return, and a later Activity may invoke `android_main()` again in the same process.
- `winit 0.30.13` receives Android's Destroy event but does not exit its Android event loop for that event.
- winit also normally rejects creating another event loop in the same process.
- A temporary local winit patch proved the diagnosis: exiting on Destroy let Android finish the old Activity, and allowing a subsequent Android event loop let the retained process create a fresh drawable Activity successfully.
- Current upstream winit was inspected during the investigation and still contained the Android Destroy TODO.

Rather than maintain a private winit fork for fundamental Android lifecycle semantics, Waker owns the small Android adapter directly. This decision is Android-specific; desktop Waker continues to use eframe/winit.

## Why GameActivity instead of NativeActivity

The first direct backend used `android.app.NativeActivity`. Its lifecycle worked correctly once winit was removed, and that version passed surface recreation, Activity recreation, lock/unlock, fold/unfold, touch and log-viewer tests.

NativeActivity's weak point is text input. `android-activity 0.6.1` exposes `show_soft_input()` and hardware-style key events for NativeActivity, but its `set_text_input_state()` and `set_ime_editor_info()` implementations are explicitly unsupported/no-ops. There is no real editable `InputConnection` for Android's keyboard to manage.

That forced Waker to infer keyboard state from egui focus and translate key events itself. A real-device test exposed exactly the kind of failure that approach invites: transient egui/keyboard-inset frames caused repeated show/hide requests and visible keyboard flicker.

GameActivity fixes the abstraction boundary rather than adding another timing workaround:

- AndroidX GameActivity is based on AppCompatActivity.
- Its GameTextInput integration owns a proper Android input connection.
- `android-activity` can publish the focused egui field's complete text, selection and composition state to GameTextInput.
- GameTextInput returns text-state changes through `InputEvent::TextEvent`, so Waker applies an editor state rather than pretending soft-keyboard input is a sequence of physical keys.
- IME purpose/options and editor actions are supported.

Waker therefore uses `android-activity 0.6.1` with its `game-activity` feature and pins the compatible AndroidX `androidx.games:games-activity:4.4.0`. Upstream Prefab support is deliberately **not** enabled: `android-activity` compiles its own GameActivity/GameTextInput native glue and documents the upstream Prefab glue as incompatible with that path.

## Direct Android lifecycle

`crates/waker-app/src/android.rs` owns the Android event loop.

The lifecycle rules are:

- `InitWindow`: obtain the current `ANativeWindow`, create/configure a wgpu surface, and request a repaint.
- `TerminateWindow`: drop only the wgpu surface. The egui context and GPU device may remain alive while the Activity temporarily has no drawable window.
- `WindowResized`, inset/configuration changes, redraw requests and focus changes: update/repaint against the current native window.
- `Destroy`: drop Activity-owned render state and return from `android_main()`.
- A later GameActivity can invoke `android_main()` again in the same retained process and construct a fresh egui/Android loop.

Touch and hardware-key events are consumed directly from `AndroidApp::input_events_iter()` and translated to egui events. Soft-keyboard editing uses GameTextInput state instead of the NativeActivity key-event workaround.

wgpu receives the Android raw display marker plus the current `ANativeWindow` handle directly. No winit window or event loop exists on Android.

## Text-input ownership

The three editable Settings fields remain ordinary egui `TextEdit` widgets. On Android, the shared UI additionally exposes the focused field's current text and character selection to the platform adapter.

The adapter synchronises that state into GameTextInput and shows the Android IME when an editable field gains focus. GameActivity then reports updated `TextInputState` values; Waker replaces the corresponding field and restores the returned selection on the next egui frame.

GameTextInput state synchronisation is deliberately edge-triggered. Calling `set_text_input_state()` is not a harmless per-frame setter: on the target device it causes GameTextInput/Android to restart the editor input connection. An early GameActivity prototype called it on every repaint and produced a rapid `APP_CALLED_RESTART_INPUT_API` stream. Waker now records the last text/selection supplied by Android and only pushes state back when egui has genuinely changed the field or selection. IME-originated composition/text events update that baseline and are not echoed back.

This keeps responsibilities separated:

- egui owns the Waker widget and its visible state;
- GameTextInput owns the Android IME/InputConnection protocol;
- the Waker Android adapter synchronises the two;
- Android, rather than a Waker timing heuristic, manages the keyboard connection and composition lifecycle.

## Process-lifetime state

Android may destroy and recreate an Activity without killing Waker's Linux process. Anything genuinely process-global must therefore outlive one `android_main()` invocation.

Persistent diagnostics are initialised once with a `OnceLock<DiagnosticsRuntime>`. The tracing subscriber and non-blocking log `WorkerGuard` are reused by later Activity instances instead of being dropped and unsuccessfully reinitialised.

Ordinary UI state is Activity-lifetime and is rebuilt for a new Activity. Network worker state remains owned by its WakerApp instance; Android does not try to resurrect a half-destroyed UI object.

## Configuration changes

The final Gradle manifest handles the target-SDK-36 set of configuration changes in place, mirroring cargo-apk2's `config_changes = "allKnown"` expansion. This includes `uiMode`, density, font scale, locale/layout direction, orientation, screen size, smallest screen size and screen layout.

This is no longer a correctness workaround for winit: the direct backend can survive genuine Activity destruction/recreation. It is retained to avoid unnecessary Activity churn and preserve current UI state across routine foldable/display/configuration changes. Those changes arrive as configuration/inset/resize events and cause a repaint.

The list can be narrowed later if a configuration is found that is better handled by full Activity recreation.

## Android packaging

GameActivity is an AndroidX AAR dependency, so the final APK needs Gradle dependency resolution. `cargo-apk2` remains the native Rust build driver, but its APK is only an intermediate build product.

`scripts/build-release-android.sh` performs the release pipeline:

1. cargo-apk2 builds the Android Rust `cdylib` and provides the canonical versionName/versionCode derived from the Cargo package version;
2. its intermediate APK is moved away from the release output path so a later packaging failure cannot leave an incomplete APK looking like a finished release;
3. the Rust `libwaker_app.so` is copied into the Gradle package's JNI inputs;
4. the checksum-pinned Gradle 9.6.1 wrapper resolves GameActivity 4.4.0 plus its AppCompat/Core dependencies and assembles the final Android application;
5. Waker's existing release key signs a temporary APK, verifies that signature, then publishes it atomically as `target/release/apk/waker_app.apk`.

The development GhostBSD host's installed Android Build Tools contain native FreeBSD replacements sufficient for cargo-apk2, but AGP validates a complete official Build Tools distribution. The helper therefore keeps a checksum-pinned official Build Tools 36 package under ignored `target/` solely for the Gradle build environment, while explicitly overriding AAPT2 with the working native FreeBSD binary. It does not modify the installed SDK.

## Validation

The winit-free direct lifecycle was validated on the target Android foldable before the GameActivity switch:

- fresh launch produced a real drawable Vulkan/wgpu window;
- ordinary surface loss/recreation worked;
- Back/finish followed by relaunch worked repeatedly in the same retained process;
- lock/unlock and fold/unfold worked;
- touch interaction and Open log worked;
- persistent diagnostics survived Activity recreation.

The GameActivity build was then validated on the same device on 12 September 2026:

- the signed `0.1.4` APK launched `app.waker.android.WakerActivity`; GameActivity loaded `libwaker_app.so` from `android.app.lib_name`, and Waker reached `mHasSurface=true`, `HAS_DRAWN`, visible/on-screen;
- focusing a Settings field created a real `com.google.androidgamesdk.gametextinput.InputConnection` and showed the system keyboard;
- manually hiding the keyboard left that input connection served, and tapping the still-focused field explicitly reopened the keyboard;
- after removing per-frame state publication, the input-restart count remained unchanged while a real soft-keyboard tap produced `setComposingText("a")`, and a real spacebar tap produced `finishComposingText` plus `commitText`;
- those IME-originated edits did not cause Waker to publish another GameTextInput state reset;
- backgrounding Waker destroyed its GameActivity surface; bringing it forward recreated a drawable surface in the same process;
- Back genuinely finished the Waker Activity and exited that `android_main()`; relaunch created a fresh Activity/window/surface in the same retained PID;
- Open log launched Android's chooser, and Back returned to the same Waker PID with a visible `HAS_DRAWN` surface.

After any GameActivity/input change, additionally verify:

1. a text field opens the keyboard without flicker;
2. typing, deletion and cursor/selection changes update the egui field correctly;
3. manually hiding the keyboard and tapping the focused field can reopen it;
4. Unicode/composition input does not corrupt the field;
5. the lifecycle tests above still pass with GameActivity.

The normal Rust test suite, strict Clippy, formatting, `git diff --check`, Gradle packaging and APK signature verification are required before deployment.

## Maintenance

Treat `android-activity` events as the authority for Android lifecycle semantics. Do not add winit back to the Android target merely for window/input conveniences; that would reintroduce a second lifecycle owner.

When updating `android-activity`, GameActivity, egui or wgpu, specifically retest:

1. normal foreground/background surface loss and recreation;
2. Back/finish followed by a fresh Activity in the same process;
3. lock/unlock;
4. folded/unfolded or other size/configuration changes;
5. touch interaction and GameTextInput/Settings text entry;
6. persistent diagnostics across repeated Activity recreation;
7. Open log and return to Waker.

If a future failure presents as “resumed but not drawing”, capture ActivityManager, WindowManager, SurfaceFlinger and Waker logs before restarting the app. First distinguish an Activity lifecycle failure from a valid surface with a renderer/input problem.