#!/usr/bin/env bash
# 构建 Android 应用（不用 Gradle）：核心交叉编译成 libnexus_android.so，外壳用 javac、d8、aapt2 打包，
# 用调试密钥签名。另外编译 Android 上的 nexus 命令行（测试时经 adb 运行）。
#
# 用法：tools/build-android.sh [x86_64|arm64-v8a ...]     默认 x86_64（redroid）
# 需要：ANDROID_HOME（platforms;android-34、build-tools;34.0.0、ndk）、JDK 17、rustup 的 Android 目标。
# 产物：target/android/nexus.apk，target/android/<abi>/nexus
set -euo pipefail
cd "$(dirname "$0")/.."
ABIS=("${@:-x86_64}")
SDK=${ANDROID_HOME:-$HOME/android-sdk}
NDK=${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* | sort -V | tail -1)}
BT=$SDK/build-tools/34.0.0
JAR=$SDK/platforms/android-34/android.jar
API=26
LLVM=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin
OUT=target/android
rm -rf "$OUT/apk" && mkdir -p "$OUT/apk/lib" "$OUT/classes"

for abi in "${ABIS[@]}"; do
  case $abi in
    x86_64) triple=x86_64-linux-android ;;
    arm64-v8a) triple=aarch64-linux-android ;;
    *) echo "不认识的 ABI：$abi"; exit 1 ;;
  esac
  up=$(echo "$triple" | tr 'a-z-' 'A-Z_')
  export "CARGO_TARGET_${up}_LINKER=$LLVM/${triple}${API}-clang"
  export "CC_${triple//-/_}=$LLVM/${triple}${API}-clang"
  export "AR_${triple//-/_}=$LLVM/llvm-ar"
  cargo build --release --target "$triple" -p nexus-android -p nexus-cli --bin nexus --lib
  mkdir -p "$OUT/apk/lib/$abi" "$OUT/$abi"
  cp "target/$triple/release/libnexus_android.so" "$OUT/apk/lib/$abi/"
  cp "target/$triple/release/nexus" "$OUT/$abi/nexus"
  "$LLVM/llvm-strip" "$OUT/apk/lib/$abi/libnexus_android.so" "$OUT/$abi/nexus"
done

javac -nowarn -Xlint:-options -source 8 -target 8 -bootclasspath "$JAR" -classpath "$JAR" -d "$OUT/classes" \
  $(find android/src -name '*.java')
"$BT/d8" --min-api $API --lib "$JAR" --output "$OUT/apk" $(find "$OUT/classes" -name '*.class')
"$BT/aapt2" link -o "$OUT/unsigned.apk" --manifest android/AndroidManifest.xml -I "$JAR" \
  --min-sdk-version $API --target-sdk-version 34
# 把 dex 和本地库加进 aapt2 给出的包（不依赖 zip 命令）。
python3 - "$OUT/unsigned.apk" "$OUT/apk" <<'PY'
import os, sys, zipfile
apk, root = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    for d, _, files in os.walk(root):
        for f in sorted(files):
            path = os.path.join(d, f)
            z.write(path, os.path.relpath(path, root))
PY
"$BT/zipalign" -f -p 4 "$OUT/unsigned.apk" "$OUT/aligned.apk"
KEY=$OUT/debug.keystore
[ -f "$KEY" ] || keytool -genkeypair -keystore "$KEY" -storepass android -keypass android -alias debug \
  -dname "CN=Nexus Debug" -keyalg RSA -keysize 2048 -validity 10000 >/dev/null 2>&1
"$BT/apksigner" sign --ks "$KEY" --ks-pass pass:android --out "$OUT/nexus.apk" "$OUT/aligned.apk"
echo "$OUT/nexus.apk"
