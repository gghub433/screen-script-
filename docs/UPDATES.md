# Automatic updates (Android)

## Flow

```
GitHub Release (tag vX.Y.Z)                       phone
  revizor-0.1.0-57.apk            ◄── GET /repos/<owner>/<repo>/releases/latest   (WorkManager ≈ every 12 h on Wi-Fi, and on app start)
  revizor-0.1.0-57.apk.sha256     ◄── versionCode 57 > installed? → "Update available"
                                  ──► download (HTTPS, GitHub hosts only, ≤ 200 MB) → verify SHA-256
                                  ──► PackageInstaller session → Android confirms → app restarts on the new version
```

* The repository is `BuildConfig.UPDATE_REPO` (default `gghub433/screen-script-`; override with
  `-Previzor.updateRepo=owner/repo`). The repository must be **public** (the check is an unauthenticated API call).
* Asset names are the contract: `revizor-<versionName>-<versionCode>.apk` plus the same name with `.sha256`.
  `release.yml` and `tools/termux-build.sh` produce exactly this.
* Settings → Updates: "Check for updates" (default on) and "Install updates automatically" (default off).

## What "automatic" can and cannot mean on Android

Android does not allow an ordinary app to replace itself silently.

* The user must allow **Install unknown apps** for Revizor once (the app takes you to that screen).
* Android shows its install confirmation. From the second update on, on Android 12+, an app that is the *installer of
  record* may update itself without a prompt (`USER_ACTION_NOT_REQUIRED`), which Revizor requests; whether the OS honours
  it depends on the Android version and OEM.
* "Install updates automatically" downloads and hands over to the installer without you opening the app; with the
  prompt above that means one tap. True zero-touch updates are only possible with root, device-owner mode or a store.

## Safety

SHA-256 must match the published checksum, and Android only accepts the APK if it is signed with the same key as the
installed app. **Use one signing key for every build** (Termux builds and CI builds are different keys unless you put the same
`release.jks` in both — pick one source of builds, or share the keystore). See SECURITY.md.

## Disabling

Settings → Updates → turn off "Check for updates": the periodic job is cancelled and the app makes no internet
requests at all.
