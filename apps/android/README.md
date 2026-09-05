# camrtsp Android

This is a Camera2-to-MediaCodec foreground-service application for API 26+.
It starts the Rust RTSP server in-process through JNI; no media process, shell
utility, or native media library is used.

On Android 13+ (API 33) the app requests `CAMERA` and `POST_NOTIFICATIONS`
before starting the foreground service.

Build the Rust shared libraries, then build the APK with the committed Gradle
wrapper (Gradle 8.13):

```bash
scripts/build-android.sh
cd apps/android
./gradlew assembleDebug
```

`scripts/build-android.sh` needs `cargo-ndk` and Android NDK `27.1.12297006`
(or `ANDROID_NDK_HOME`). It emits `arm64-v8a` and `x86_64` libraries under
`app/src/main/jniLibs`. Those libraries are gitignored; regenerate them
before assembling.

Release signing is deliberately supplied by CI, not committed to the
repository.
