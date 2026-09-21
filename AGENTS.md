# Waker repository notes

## Android debuggability is intentional

- Keep `android:debuggable="true"` in `android-gradle/app/src/main/AndroidManifest.xml`, including for release APKs.
- Android release builds are deliberately debuggable so ADB/log retrieval and on-device diagnostics remain available during development and testing.
- Do not remove the attribute merely to satisfy Android Lint's `HardcodedDebugMode` warning.
- Suppress only `HardcodedDebugMode` in the Gradle Android lint configuration; keep other release lint checks enabled.
- A release APK here means release/optimized native code; it remains intentionally debuggable unless the user explicitly changes this policy.
