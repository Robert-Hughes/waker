# Android NativeActivity lifecycle workaround

Waker currently uses eframe/winit on Android through `android.app.NativeActivity`.

## Why `configChanges = "allKnown"` is required

On 11 September 2026, Waker was captured in a broken resume state after the phone had been locked and unlocked. Android considered the activity resumed, but the actual NativeActivity window remained hidden and had no surface. The splash screen stayed visible indefinitely.

The system log showed the sequence that caused it:

1. Android detected a `CONFIG_UI_MODE` change while bringing Waker back to the foreground.
2. Waker's manifest did not claim to handle `uiMode`, so Android chose to relaunch the NativeActivity.
3. WindowManager later reported that the Waker ActivityRecord was relaunching while the splash was ready to show and the real NativeActivity had `NO_SURFACE`.
4. The relaunch never completed.

This matches the lifecycle behaviour of the versions currently in use:

- `android-activity 0.6.1` sends `MainEvent::Destroy` and its NativeActivity destroy callback waits for the Rust `android_main` thread to stop.
- `winit 0.30.13` receives `MainEvent::Destroy` but does not exit the Android event loop; that branch currently only logs a TODO.
- As a result, a framework-requested NativeActivity recreation can deadlock with Android waiting for the old native thread to terminate.

The workaround is therefore declared in `crates/waker-app/Cargo.toml`:

```toml
config_changes = "allKnown"
```

With the current target SDK (36), the Android build tools expand `allKnown` to the configuration changes known to that target, including `uiMode`, density, font scale, locale/layout direction, orientation, screen size, smallest screen size, and screen layout. This causes those changes to be delivered to the existing NativeActivity instead of asking Android to recreate it.

Using `allKnown` is intentional. Handling only `uiMode` would fix the captured incident but leave the same relaunch deadlock available for another configuration change, which is particularly relevant on a foldable device.

## Validation

The workaround was tested on the target Samsung Fold device:

- a forced night-mode transition from night to day and back to night kept Waker in the same process and produced no Activity relaunch;
- the phone's night-mode preference was restored to `auto`;
- a real lock/unlock cycle then resumed Waker successfully;
- the Activity was `RESUMED`, visible, and reported drawn;
- the NativeActivity window had a live surface and `HAS_DRAWN`;
- a new surface was created and Waker submitted its first frame;
- Android removed the splash screen normally;
- no `Checking to restart ... not-handles` or `is relaunching` marker appeared in the successful run.

## Maintenance

Do not remove this manifest setting merely because normal suspend/resume works in a short test. Remove or narrow it only after the Android event-loop stack has been updated to a version that safely supports NativeActivity destruction/recreation, and after that path has been verified on-device.

If the app is ever observed as logically resumed while its real window has no surface and the splash screen remains visible, check Android system logs for Activity relaunch/configuration-change messages before treating it as a renderer failure.
