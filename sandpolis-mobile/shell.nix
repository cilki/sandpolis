{ pkgs ? import ../nix/nixpkgs.nix {
  config.allowUnfree = true;
  overlays = [
    (import (builtins.fetchTarball
      "https://github.com/oxalica/rust-overlay/archive/master.tar.gz"))
  ];
} }:

with pkgs;

let
  # Android SDK with minimal components needed for building APKs
  android-nixpkgs = callPackage (fetchTarball {
    url =
      "https://github.com/tadfisher/android-nixpkgs/archive/9df60dbd7d9a31b33e564632e257b4a0dcd84fda.tar.gz";
    sha256 = "0x96imzfw9627xl3izijahy51rq1r8f1nvsfh4vnmy7nilrv0qaz";
  }) { channel = "stable"; };

  android-sdk = android-nixpkgs.sdk (sdkPkgs:
    with sdkPkgs; [
      # Not cmdline-tools-latest: recent versions replace the Java sdkmanager
      # with a native `android` binary that isn't patchelf'd for nix
      cmdline-tools-17-0 # SDK manager
      build-tools-34-0-0 # Required by gradle
      platforms-android-34 # API 34 for compilation
      ndk-26-1-10909125 # Native compilation for Rust
    ]);

  # Rust toolchain with Android targets
  rust-android = rust-bin.stable.latest.default.override {
    extensions = [ "rust-src" ];
    targets = [ "aarch64-linux-android" ];
  };

in mkShell {
  buildInputs = [
    android-sdk
    jdk17 # Required by gradle
    gradle # APK assembly
    cargo-ndk # Builds Rust for Android targets
    rust-android # Rust with Android targets
    cmake # Required by aws-lc-sys for crypto
    pkg-config # Required for library detection
    zlib # System zlib to avoid NDK build issues
    clang # Host C compiler for build scripts
    llvmPackages.bintools # Host linker
  ];

  # Android SDK/NDK environment variables
  ANDROID_HOME = "${android-sdk}/share/android-sdk";
  ANDROID_SDK_ROOT = "${android-sdk}/share/android-sdk";
  ANDROID_NDK_ROOT = "${android-sdk}/share/android-sdk/ndk/26.1.10909125";
  NDK_HOME = "${android-sdk}/share/android-sdk/ndk/26.1.10909125";

  # Configure cargo-ndk to use system zlib instead of building it
  ZLIB_SYS_STATIC = "0";
  PKG_CONFIG_ALLOW_CROSS = "1";

  GRADLE_OPTS =
    "-Dorg.gradle.project.android.aapt2FromMavenOverride=${android-sdk}/share/android-sdk/build-tools/34.0.0/aapt2";

  shellHook = ''
    # Generate local.properties with Nix store paths
    cat <<-EOF > android/local.properties 
      sdk.dir=$ANDROID_SDK_ROOT
      ndk.dir=$ANDROID_NDK_ROOT
    EOF

    # Set host compilers for build scripts (before cargo-ndk overrides them)
    export CC_x86_64_unknown_linux_gnu=${pkgs.clang}/bin/clang
    export CXX_x86_64_unknown_linux_gnu=${pkgs.clang}/bin/clang++
    export AR_x86_64_unknown_linux_gnu=${pkgs.llvmPackages.bintools}/bin/ar

    echo "Don't enter this shell from the parent's development shell!"
    echo "Rust version: $(rustc --version)"
    echo ""
    echo "To build APK:"
    echo "  # Debug build:"
    echo "  cargo ndk -t arm64-v8a --platform 31 -o android/app/src/main/jniLibs build --link-libcxx-shared"
    echo "  cd android && ./gradlew assembleDebug"
    echo ""
    echo "  # Release build:"
    echo "  cargo ndk -t arm64-v8a --platform 31 -o android/app/src/main/jniLibs build --release --link-libcxx-shared"
    echo "  cd android && ./gradlew assembleRelease"
    echo ""
    echo "APK output: android/app/build/outputs/apk/"
  '';
}

