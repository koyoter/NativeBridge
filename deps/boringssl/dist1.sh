DIST_ROOT=$1
LIB_NAME=boringssl
DIST_DIR="${DIST_ROOT}/${LIB_NAME}"

dist_lib ${LIB_NAME} ${DIST_DIR} $DISTF_ALL

create_xcfraemwork bssl-ssl ${LIB_NAME} libssl.a
create_xcfraemwork bssl-crypto ${LIB_NAME} libcrypto.a
