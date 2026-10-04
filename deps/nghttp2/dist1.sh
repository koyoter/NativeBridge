DIST_ROOT=$1
LIB_NAME=nghttp2
DIST_DIR="${DIST_ROOT}/${LIB_NAME}"

dist_lib ${LIB_NAME} ${DIST_DIR} $DISTF_NATIVES

# nghttp2 1.70.0 keys off BUILD_STATIC_LIBS/BUILD_SHARED_LIBS (the old
# ENABLE_STATIC_LIB/ENABLE_SHARED_LIB flags were removed upstream). build.yml now
# forces static-only, so libnghttp2.a / nghttp2.lib are produced and distributed
# by dist_lib above; there is no shared lib and no xcframework for nghttp2.
