DIST_ROOT=$1
LIB_NAME=ngtcp2
DIST_DIR="${DIST_ROOT}/${LIB_NAME}"

dist_lib ${LIB_NAME} ${DIST_DIR} $DISTF_NATIVES

create_xcfraemwork ngtcp2 ${LIB_NAME} libngtcp2.a
create_xcfraemwork ngtcp2_crypto_boringssl ${LIB_NAME} libngtcp2_crypto_boringssl.a
