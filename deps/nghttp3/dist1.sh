DIST_ROOT=$1
LIB_NAME=nghttp3
DIST_DIR="${DIST_ROOT}/${LIB_NAME}"

dist_lib ${LIB_NAME} ${DIST_DIR} $DISTF_NATIVES

create_xcfraemwork nghttp3 ${LIB_NAME} libnghttp3.a
