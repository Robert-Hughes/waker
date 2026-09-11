# Android release signing

Waker release APKs use a project-specific signing key kept locally under the repository root:

```text
.signing/waker-release.keystore
.signing/release.env
```

The entire `.signing/` directory is ignored by Git. Both files should remain private and should be backed up securely outside the repository. Losing the keystore or its password means future APKs cannot update an installed Waker release signed with this key.

The release key was created on 11 September 2026 with alias `waker-release`. Its public certificate SHA-256 fingerprint is:

```text
0E:42:AA:52:64:3E:63:02:B1:E6:E3:71:AE:6F:5F:35:28:6C:B0:06:B3:24:21:80:86:50:34:91:7C:FE:CF:3B
```

The private password is deliberately not documented or committed. The local `.signing/release.env` exports the cargo-apk2 variables `CARGO_APK_RELEASE_KEYSTORE` and `CARGO_APK_RELEASE_KEYSTORE_PASSWORD`.

## Build

On Manta/GhostBSD:

```sh
cd ~/src/waker
./scripts/build-release-android.sh
```

The signed APK is produced at:

```text
target/release/apk/waker_app.apk
```

The build helper sources the ignored signing environment and invokes the native FreeBSD Android build wrapper with `--release`.
cargo-apk2 derives Android `versionCode` from the Cargo package version. **Bump `[workspace.package].version` for every APK intended to update an installed build**; otherwise some Android package installers may reject the sideload as not being a newer version.

## First migration from a debug build

Android requires updates to a package to carry the same signing certificate. The development APKs installed before this release were signed with Manta's Android debug key, while production releases use the Waker-specific release key above.

Therefore the first production-key installation cannot update the existing debug-signed `app.waker.android` in place. Uninstalling the debug build also deletes its app-private `waker.local.conf`.

Before the first production install:

1. preserve/recover the private Waker profile;
2. uninstall the debug-signed Waker;
3. install the release-signed APK;
4. provision `waker.local.conf` back into the new app-private data directory;
5. verify a real wake and **Copy diagnostics**.

After that migration, future APKs signed with the same Waker release key can update the production installation normally.

## Key backup

Back up both of these together:

- `.signing/waker-release.keystore`
- `.signing/release.env`

Do not put either file in Git, shared diagnostics, issue attachments, or public cloud folders without appropriate encryption.
