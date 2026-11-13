#!/bin/sh
# Builds ESP-IDF's protocomm Security 2 handler (security2.c, esp_srp.c) for
# this computer, as the device side of the login test:
#
#   tests/e2e/srp_device/build.sh /tmp/srp
#   SIDP_SRP_DEVICE=/tmp/srp/srp-device cargo test --release login -- --include-ignored
#
# Needs IDF_PATH, a C compiler, CMake, and Python with jsonschema and jinja2
# (for mbedtls' generated files).
set -e
: "${IDF_PATH:?set IDF_PATH}"
OUT=$(realpath -m "${1:?usage: build.sh OUTPUT_DIR}")
HERE=$(dirname "$(realpath "$0")")
C=$IDF_PATH/components
M=$C/mbedtls/mbedtls
mkdir -p "$OUT/include/mbedtls"

# ESP-IDF's mbedtls port headers wrap the private ones; do the same.
for h in "$C"/mbedtls/port/include/mbedtls/*.h; do
    n=$(basename "$h")
    if [ -f "$M/tf-psa-crypto/drivers/builtin/include/mbedtls/private/$n" ]; then
        printf '#pragma once\n#define MBEDTLS_DECLARE_PRIVATE_IDENTIFIERS\n#include "mbedtls/private/%s"\n' "$n" \
            > "$OUT/include/mbedtls/$n"
    fi
done

export PYTHONPATH=$M/framework/scripts:$M/tf-psa-crypto/framework/scripts
cmake -S "$M/tf-psa-crypto" -B "$OUT/crypto" -DCMAKE_C_FLAGS="-I$OUT/include" \
    -DENABLE_TESTING=OFF -DENABLE_PROGRAMS=OFF -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build "$OUT/crypto" -j >/dev/null

LIBS=$(find "$OUT/crypto" -name 'lib*.a')
cc -O1 -include "$HERE/stub/sdkconfig.h" -o "$OUT/srp-device" "$HERE/main.c" \
    "$C/protocomm/src/security/security2.c" \
    "$C/protocomm/src/crypto/srp6a/esp_srp.c" "$C/protocomm/src/crypto/srp6a/esp_srp_mpi.c" \
    "$C"/protocomm/proto-c/*.pb-c.c "$C/protobuf-c/protobuf-c/protobuf-c/protobuf-c.c" \
    -I"$HERE/stub" -I"$OUT/include" \
    -I"$C/protocomm/include/security" -I"$C/protocomm/include/common" \
    -I"$C/protocomm/include/crypto/srp6a" -I"$C/protocomm/proto-c" \
    -I"$C/protocomm/src/crypto/srp6a" -I"$C/protocomm/src/common" -I"$C/protobuf-c/protobuf-c" \
    -I"$M/tf-psa-crypto/include" -I"$M/tf-psa-crypto/drivers/builtin/include" -I"$M/include" \
    -I"$OUT/crypto/include" $LIBS $LIBS 2>/dev/null
echo "$OUT/srp-device"
