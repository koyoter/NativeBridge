//
// Curlw.cs — C# binding for the NativeBridge "curlw" module.
//
// This is the managed mirror of the native contract exported by
//   src/nativebridge/rust/src/modules/curlw/mod.rs
// Keep the two in lock-step: every P/Invoke below matches a #[no_mangle]
// export 1:1 (cdecl calling convention, same argument widths).
//
// Rationale: libcurl's curl_easy_setopt / curl_easy_getinfo are variadic, which
// P/Invoke marshals unreliably. The native curlw layer exposes fixed-arity typed
// variants; this file exposes them to Unity.
//
// Design notes:
//   * namespace NativeBridgeF, LIBNAME "NativeBridge" (native library file name).
//   * CURLMsg is read via curlw_msg_* accessors, not a mirrored struct layout.
//   * setopt long vs off_t are distinct (curlw_easy_setopt_long / _offt).
//   * strings cross the ABI as UTF-8: string params are encoded via Utf8Bytes
//     (NUL-terminated byte[]) and returned char* values are read via
//     PtrToStringUtf8. Never use CharSet.Ansi — on Windows it converts through
//     the system ANSI code page (e.g. GBK) while libcurl expects UTF-8.
//
#if !UNITY_WEBGL
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using CURLH = System.IntPtr;   // CURL*  (easy handle)
using CURLMH = System.IntPtr;  // CURLM* (multi handle)

namespace NativeBridgeF
{
    public static class CURLDefines
    {
        public const int CURLINFO_STRING = 0x100000;
        public const int CURLINFO_LONG = 0x200000;
        public const int CURLINFO_DOUBLE = 0x300000;
        public const int CURLINFO_SLIST = 0x400000;
        public const int CURLINFO_PTR = 0x400000; /* same as SLIST */
        public const int CURLINFO_SOCKET = 0x500000;
        public const int CURLINFO_OFF_T = 0x600000;
        public const int CURLINFO_MASK = 0x0fffff;
        public const int CURLINFO_TYPEMASK = 0xf00000;

        public const int CURLOPTTYPE_LONG = 0;
        public const int CURLOPTTYPE_OBJECTPOINT = 10000;
        public const int CURLOPTTYPE_FUNCTIONPOINT = 20000;
        public const int CURLOPTTYPE_OFF_T = 30000;
        public const int CURLOPTTYPE_BLOB = 40000;
        public const int CURLOPTTYPE_CBPOINT = CURLOPTTYPE_OBJECTPOINT;
        public const int CURLOPTTYPE_STRINGPOINT = CURLOPTTYPE_OBJECTPOINT;
        public const int CURLOPTTYPE_SLISTPOINT = CURLOPTTYPE_OBJECTPOINT;
        public const int CURLOPTTYPE_VALUES = CURLOPTTYPE_LONG;

        public const int CURL_GLOBAL_SSL = (1 << 0); /* no purpose since 7.57.0 */
        public const int CURL_GLOBAL_WIN32 = (1 << 1);
        public const int CURL_GLOBAL_ALL = (CURL_GLOBAL_SSL | CURL_GLOBAL_WIN32);
        public const int CURL_GLOBAL_NOTHING = 0;
        public const int CURL_GLOBAL_DEFAULT = CURL_GLOBAL_ALL;
        public const int CURL_GLOBAL_ACK_EINTR = (1 << 2);

        // --- Tier 1 value/bit-flag constants (curl 8.21.0) -------------------
        // Pass these as the optval to curlw_easy_setopt_long for the matching
        // CURLOPTTYPE_VALUES / LONG options.

        // CURLOPT_IPRESOLVE
        public const int CURL_IPRESOLVE_WHATEVER = 0;
        public const int CURL_IPRESOLVE_V4 = 1;
        public const int CURL_IPRESOLVE_V6 = 2;

        // CURLOPT_SSLVERSION (min); OR with a CURL_SSLVERSION_MAX_* to cap.
        public const int CURL_SSLVERSION_DEFAULT = 0;
        public const int CURL_SSLVERSION_TLSv1 = 1;
        public const int CURL_SSLVERSION_TLSv1_0 = 4;
        public const int CURL_SSLVERSION_TLSv1_1 = 5;
        public const int CURL_SSLVERSION_TLSv1_2 = 6;
        public const int CURL_SSLVERSION_TLSv1_3 = 7;
        public const int CURL_SSLVERSION_MAX_DEFAULT = (CURL_SSLVERSION_TLSv1 << 16);
        public const int CURL_SSLVERSION_MAX_TLSv1_0 = (CURL_SSLVERSION_TLSv1_0 << 16);
        public const int CURL_SSLVERSION_MAX_TLSv1_1 = (CURL_SSLVERSION_TLSv1_1 << 16);
        public const int CURL_SSLVERSION_MAX_TLSv1_2 = (CURL_SSLVERSION_TLSv1_2 << 16);
        public const int CURL_SSLVERSION_MAX_TLSv1_3 = (CURL_SSLVERSION_TLSv1_3 << 16);

        // CURLOPT_PROXYTYPE
        public const int CURLPROXY_HTTP = 0;
        public const int CURLPROXY_HTTP_1_0 = 1;
        public const int CURLPROXY_HTTPS = 2;
        public const int CURLPROXY_HTTPS2 = 3;
        public const int CURLPROXY_SOCKS4 = 4;
        public const int CURLPROXY_SOCKS5 = 5;
        public const int CURLPROXY_SOCKS4A = 6;
        public const int CURLPROXY_SOCKS5_HOSTNAME = 7;

        // CURLOPT_ALTSVC_CTRL bitmask
        public const int CURLALTSVC_READONLYFILE = (1 << 2);
        public const int CURLALTSVC_H1 = (1 << 3);
        public const int CURLALTSVC_H2 = (1 << 4);
        public const int CURLALTSVC_H3 = (1 << 5);

        // CURLOPT_HTTPAUTH bitmask (unsigned long in curl; pass via setopt_long)
        public const long CURLAUTH_NONE = 0;
        public const long CURLAUTH_BASIC = (1L << 0);
        public const long CURLAUTH_DIGEST = (1L << 1);
        public const long CURLAUTH_NEGOTIATE = (1L << 2);
        public const long CURLAUTH_NTLM = (1L << 3);
        public const long CURLAUTH_DIGEST_IE = (1L << 4);
        public const long CURLAUTH_BEARER = (1L << 6);
        // libcurl defines CURLAUTH_ANY as ~CURLAUTH_DIGEST_IE (all bits except bit 4).
        // We narrow to a 32-bit mask so the value fits Win64's 32-bit native `long`
        // via curlw_try_native_long's unsigned-32 allowance. curl 8.x keeps every
        // auth bit within bit 0..6, so this covers all defined methods; revisit if
        // libcurl ever introduces auth bits above bit 31.
        public const long CURLAUTH_ANY = 0xffffffffL & ~CURLAUTH_DIGEST_IE;
    }

    /// <summary>
    /// Common curl options. Values verified against curl 8.21.0 include/curl/curl.h.
    /// Add more entries here as needed (copy the value from curl.h).
    /// </summary>
    public enum CURLoption
    {
        CURLOPT_URL                   = CURLDefines.CURLOPTTYPE_STRINGPOINT + 2,
        CURLOPT_PORT                  = CURLDefines.CURLOPTTYPE_LONG + 3,
        CURLOPT_RANGE                 = CURLDefines.CURLOPTTYPE_STRINGPOINT + 7,
        CURLOPT_VERBOSE               = CURLDefines.CURLOPTTYPE_LONG + 41,
        CURLOPT_HEADER                = CURLDefines.CURLOPTTYPE_LONG + 42,
        CURLOPT_NOPROGRESS            = CURLDefines.CURLOPTTYPE_LONG + 43,
        CURLOPT_NOBODY                = CURLDefines.CURLOPTTYPE_LONG + 44,
        CURLOPT_POST                  = CURLDefines.CURLOPTTYPE_LONG + 47,
        CURLOPT_FOLLOWLOCATION        = CURLDefines.CURLOPTTYPE_LONG + 52,
        // WARNING: libcurl does NOT copy CURLOPT_POSTFIELDS — it stores the
        // pointer and reads it later at perform() time. Do not set it via the
        // `string` overload (the marshalled buffer is freed when the P/Invoke
        // returns → dangling pointer). Use CURLOPT_COPYPOSTFIELDS (curl copies
        // the bytes), or pin/allocate a native buffer yourself and pass it via
        // the IntPtr overload, keeping it alive until the transfer completes.
        CURLOPT_POSTFIELDS            = CURLDefines.CURLOPTTYPE_OBJECTPOINT + 15,
        CURLOPT_COPYPOSTFIELDS        = CURLDefines.CURLOPTTYPE_OBJECTPOINT + 165,
        CURLOPT_USERAGENT             = CURLDefines.CURLOPTTYPE_STRINGPOINT + 18,
        CURLOPT_LOW_SPEED_LIMIT       = CURLDefines.CURLOPTTYPE_LONG + 19,
        CURLOPT_LOW_SPEED_TIME        = CURLDefines.CURLOPTTYPE_LONG + 20,
        CURLOPT_SSL_VERIFYPEER        = CURLDefines.CURLOPTTYPE_LONG + 64,
        CURLOPT_CAINFO                = CURLDefines.CURLOPTTYPE_STRINGPOINT + 65,
        CURLOPT_MAXCONNECTS           = CURLDefines.CURLOPTTYPE_LONG + 71,
        CURLOPT_POSTFIELDSIZE         = CURLDefines.CURLOPTTYPE_LONG + 60,
        CURLOPT_HTTPGET               = CURLDefines.CURLOPTTYPE_LONG + 80,
        CURLOPT_SSL_VERIFYHOST        = CURLDefines.CURLOPTTYPE_LONG + 81,
        CURLOPT_HTTP_VERSION          = CURLDefines.CURLOPTTYPE_VALUES + 84,
        CURLOPT_CUSTOMREQUEST         = CURLDefines.CURLOPTTYPE_STRINGPOINT + 36,
        CURLOPT_ACCEPT_ENCODING       = CURLDefines.CURLOPTTYPE_STRINGPOINT + 102,
        CURLOPT_BUFFERSIZE            = CURLDefines.CURLOPTTYPE_LONG + 98,
        CURLOPT_NOSIGNAL              = CURLDefines.CURLOPTTYPE_LONG + 99,
        CURLOPT_TIMEOUT_MS            = CURLDefines.CURLOPTTYPE_LONG + 155,
        CURLOPT_CONNECTTIMEOUT_MS     = CURLDefines.CURLOPTTYPE_LONG + 156,
        CURLOPT_TCP_KEEPALIVE         = CURLDefines.CURLOPTTYPE_LONG + 213,
        CURLOPT_TCP_KEEPIDLE          = CURLDefines.CURLOPTTYPE_LONG + 214,
        CURLOPT_TCP_KEEPINTVL         = CURLDefines.CURLOPTTYPE_LONG + 215,
        CURLOPT_MAX_RECV_SPEED_LARGE  = CURLDefines.CURLOPTTYPE_OFF_T + 146,
        CURLOPT_MAX_SEND_SPEED_LARGE  = CURLDefines.CURLOPTTYPE_OFF_T + 145,
        CURLOPT_INFILESIZE_LARGE      = CURLDefines.CURLOPTTYPE_OFF_T + 115,
        CURLOPT_POSTFIELDSIZE_LARGE   = CURLDefines.CURLOPTTYPE_OFF_T + 120,
        CURLOPT_HTTPHEADER            = CURLDefines.CURLOPTTYPE_SLISTPOINT + 23,
        CURLOPT_WRITEFUNCTION         = CURLDefines.CURLOPTTYPE_FUNCTIONPOINT + 11,
        CURLOPT_WRITEDATA             = CURLDefines.CURLOPTTYPE_CBPOINT + 1,
        CURLOPT_READFUNCTION          = CURLDefines.CURLOPTTYPE_FUNCTIONPOINT + 12,
        CURLOPT_READDATA              = CURLDefines.CURLOPTTYPE_CBPOINT + 9,
        CURLOPT_PRIVATE               = CURLDefines.CURLOPTTYPE_OBJECTPOINT + 103,

        // --- Tier 1 additions (values verified against curl 8.21.0 curl.h) ----
        // These reuse the existing typed setopt entry points (long / string /
        // slist), so no native changes are required.

        // DNS / connection control
        CURLOPT_DOH_URL               = CURLDefines.CURLOPTTYPE_STRINGPOINT + 279,
        CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS = CURLDefines.CURLOPTTYPE_LONG + 271,
        CURLOPT_RESOLVE               = CURLDefines.CURLOPTTYPE_SLISTPOINT + 203,
        CURLOPT_CONNECT_TO            = CURLDefines.CURLOPTTYPE_SLISTPOINT + 243,
        CURLOPT_IPRESOLVE             = CURLDefines.CURLOPTTYPE_VALUES + 113,
        CURLOPT_PIPEWAIT              = CURLDefines.CURLOPTTYPE_LONG + 237,
        CURLOPT_CONNECTTIMEOUT        = CURLDefines.CURLOPTTYPE_LONG + 78,   /* seconds */
        CURLOPT_TIMEOUT               = CURLDefines.CURLOPTTYPE_LONG + 13,   /* seconds */

        // TLS / certificates
        CURLOPT_CAPATH                = CURLDefines.CURLOPTTYPE_STRINGPOINT + 97,
        CURLOPT_SSLCERT               = CURLDefines.CURLOPTTYPE_STRINGPOINT + 25,
        CURLOPT_SSLKEY                = CURLDefines.CURLOPTTYPE_STRINGPOINT + 87,
        CURLOPT_PINNEDPUBLICKEY       = CURLDefines.CURLOPTTYPE_STRINGPOINT + 230,
        CURLOPT_SSLVERSION            = CURLDefines.CURLOPTTYPE_VALUES + 32,
        CURLOPT_ALTSVC                = CURLDefines.CURLOPTTYPE_STRINGPOINT + 287,
        CURLOPT_ALTSVC_CTRL           = CURLDefines.CURLOPTTYPE_LONG + 286,
        CURLOPT_HSTS                  = CURLDefines.CURLOPTTYPE_STRINGPOINT + 300,

        // Proxy / auth / cookies / redirects
        CURLOPT_PROXY                 = CURLDefines.CURLOPTTYPE_STRINGPOINT + 4,
        CURLOPT_PROXYTYPE             = CURLDefines.CURLOPTTYPE_VALUES + 101,
        CURLOPT_NOPROXY               = CURLDefines.CURLOPTTYPE_STRINGPOINT + 177,
        CURLOPT_USERPWD               = CURLDefines.CURLOPTTYPE_STRINGPOINT + 5,
        CURLOPT_USERNAME              = CURLDefines.CURLOPTTYPE_STRINGPOINT + 173,
        CURLOPT_PASSWORD              = CURLDefines.CURLOPTTYPE_STRINGPOINT + 174,
        CURLOPT_HTTPAUTH              = CURLDefines.CURLOPTTYPE_VALUES + 107,
        CURLOPT_COOKIE                = CURLDefines.CURLOPTTYPE_STRINGPOINT + 22,
        CURLOPT_COOKIEFILE            = CURLDefines.CURLOPTTYPE_STRINGPOINT + 31,
        CURLOPT_COOKIEJAR             = CURLDefines.CURLOPTTYPE_STRINGPOINT + 82,
        CURLOPT_MAXREDIRS             = CURLDefines.CURLOPTTYPE_LONG + 68,
        CURLOPT_FAILONERROR           = CURLDefines.CURLOPTTYPE_LONG + 45,

        // --- Tier 2 additions: callbacks (reuse the pointer setopt entry) ------
        CURLOPT_HEADERFUNCTION        = CURLDefines.CURLOPTTYPE_FUNCTIONPOINT + 79,
        CURLOPT_HEADERDATA            = CURLDefines.CURLOPTTYPE_CBPOINT + 29,
        CURLOPT_XFERINFOFUNCTION      = CURLDefines.CURLOPTTYPE_FUNCTIONPOINT + 219,
        CURLOPT_XFERINFODATA          = CURLDefines.CURLOPTTYPE_CBPOINT + 57,
        CURLOPT_DEBUGFUNCTION         = CURLDefines.CURLOPTTYPE_FUNCTIONPOINT + 94,
        CURLOPT_DEBUGDATA             = CURLDefines.CURLOPTTYPE_CBPOINT + 95,

        // --- Tier 3 additions: BLOB options (need curlw_easy_setopt_blob) ------
        CURLOPT_CAINFO_BLOB           = CURLDefines.CURLOPTTYPE_BLOB + 309,
        CURLOPT_SSLCERT_BLOB          = CURLDefines.CURLOPTTYPE_BLOB + 291,
        CURLOPT_SSLKEY_BLOB           = CURLDefines.CURLOPTTYPE_BLOB + 292,
        CURLOPT_PROXY_CAINFO_BLOB     = CURLDefines.CURLOPTTYPE_BLOB + 310,

        // Attaches a CURLSH* (from curlw_share_init) so this handle uses shared
        // caches (DNS/connection/TLS-session/...). Set via curlw_easy_setopt_pointer.
        CURLOPT_SHARE                 = CURLDefines.CURLOPTTYPE_OBJECTPOINT + 100,
    }

    // https://curl.se/libcurl/c/CURLOPT_HTTP_VERSION.html
    public enum CURLhttpVersion
    {
        CURL_HTTP_VERSION_NONE,
        CURL_HTTP_VERSION_1_0,
        CURL_HTTP_VERSION_1_1,
        CURL_HTTP_VERSION_2_0,
        CURL_HTTP_VERSION_2TLS,
        CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE,
        CURL_HTTP_VERSION_3 = 30,      /* explicit HTTP/3, no fallback */
        CURL_HTTP_VERSION_3ONLY = 31,
    }

    public enum CURLcode
    {
        CURLE_OK = 0,
        CURLE_UNSUPPORTED_PROTOCOL,    /* 1 */
        CURLE_FAILED_INIT,             /* 2 */
        CURLE_URL_MALFORMAT,           /* 3 */
        CURLE_NOT_BUILT_IN,            /* 4 */
        CURLE_COULDNT_RESOLVE_PROXY,   /* 5 */
        CURLE_COULDNT_RESOLVE_HOST,    /* 6 */
        CURLE_COULDNT_CONNECT,         /* 7 */
        CURLE_WEIRD_SERVER_REPLY,      /* 8 */
        CURLE_REMOTE_ACCESS_DENIED,    /* 9 */
        CURLE_FTP_ACCEPT_FAILED,       /* 10 */
        CURLE_FTP_WEIRD_PASS_REPLY,    /* 11 */
        CURLE_FTP_ACCEPT_TIMEOUT,      /* 12 */
        CURLE_FTP_WEIRD_PASV_REPLY,    /* 13 */
        CURLE_FTP_WEIRD_227_FORMAT,    /* 14 */
        CURLE_FTP_CANT_GET_HOST,       /* 15 */
        CURLE_HTTP2,                   /* 16 */
        CURLE_FTP_COULDNT_SET_TYPE,    /* 17 */
        CURLE_PARTIAL_FILE,            /* 18 */
        CURLE_FTP_COULDNT_RETR_FILE,   /* 19 */
        CURLE_OBSOLETE20,              /* 20 */
        CURLE_QUOTE_ERROR,             /* 21 */
        CURLE_HTTP_RETURNED_ERROR,     /* 22 */
        CURLE_WRITE_ERROR,             /* 23 */
        CURLE_OBSOLETE24,              /* 24 */
        CURLE_UPLOAD_FAILED,           /* 25 */
        CURLE_READ_ERROR,              /* 26 */
        CURLE_OUT_OF_MEMORY,           /* 27 */
        CURLE_OPERATION_TIMEDOUT,      /* 28 */
        CURLE_OBSOLETE29,              /* 29 */
        CURLE_FTP_PORT_FAILED,         /* 30 */
        CURLE_FTP_COULDNT_USE_REST,    /* 31 */
        CURLE_OBSOLETE32,              /* 32 */
        CURLE_RANGE_ERROR,             /* 33 */
        CURLE_OBSOLETE34,              /* 34 (was CURLE_HTTP_POST_ERROR) */
        CURLE_SSL_CONNECT_ERROR,       /* 35 */
        CURLE_BAD_DOWNLOAD_RESUME,     /* 36 */
        CURLE_FILE_COULDNT_READ_FILE,  /* 37 */
        CURLE_LDAP_CANNOT_BIND,        /* 38 */
        CURLE_LDAP_SEARCH_FAILED,      /* 39 */
        CURLE_OBSOLETE40,              /* 40 */
        CURLE_OBSOLETE41,              /* 41 - NOT USED starting with 7.53.0 (was CURLE_FUNCTION_NOT_FOUND) */
        CURLE_ABORTED_BY_CALLBACK,     /* 42 */
        CURLE_BAD_FUNCTION_ARGUMENT,   /* 43 */
        CURLE_OBSOLETE44,              /* 44 */
        CURLE_INTERFACE_FAILED,        /* 45 */
        CURLE_OBSOLETE46,              /* 46 */
        CURLE_TOO_MANY_REDIRECTS,      /* 47 */
        CURLE_UNKNOWN_OPTION,          /* 48 */
        CURLE_SETOPT_OPTION_SYNTAX,    /* 49 */
        CURLE_OBSOLETE50,              /* 50 */
        CURLE_OBSOLETE51,              /* 51 */
        CURLE_GOT_NOTHING,             /* 52 */
        CURLE_SSL_ENGINE_NOTFOUND,     /* 53 */
        CURLE_SSL_ENGINE_SETFAILED,    /* 54 */
        CURLE_SEND_ERROR,              /* 55 */
        CURLE_RECV_ERROR,              /* 56 */
        CURLE_OBSOLETE57,              /* 57 */
        CURLE_SSL_CERTPROBLEM,         /* 58 */
        CURLE_SSL_CIPHER,              /* 59 */
        CURLE_PEER_FAILED_VERIFICATION,/* 60 */
        CURLE_BAD_CONTENT_ENCODING,    /* 61 */
        CURLE_OBSOLETE62,              /* 62 */
        CURLE_FILESIZE_EXCEEDED,       /* 63 */
        CURLE_USE_SSL_FAILED,          /* 64 */
        CURLE_SEND_FAIL_REWIND,        /* 65 */
        CURLE_SSL_ENGINE_INITFAILED,   /* 66 */
        CURLE_LOGIN_DENIED,            /* 67 */
        CURLE_TFTP_NOTFOUND,           /* 68 */
        CURLE_TFTP_PERM,               /* 69 */
        CURLE_REMOTE_DISK_FULL,        /* 70 */
        CURLE_TFTP_ILLEGAL,            /* 71 */
        CURLE_TFTP_UNKNOWNID,          /* 72 */
        CURLE_REMOTE_FILE_EXISTS,      /* 73 */
        CURLE_TFTP_NOSUCHUSER,         /* 74 */
        CURLE_OBSOLETE75,              /* 75 */
        CURLE_OBSOLETE76,              /* 76 */
        CURLE_SSL_CACERT_BADFILE,      /* 77 */
        CURLE_REMOTE_FILE_NOT_FOUND,   /* 78 */
        CURLE_SSH,                     /* 79 */
        CURLE_SSL_SHUTDOWN_FAILED,     /* 80 */
        CURLE_AGAIN,                   /* 81 */
        CURLE_SSL_CRL_BADFILE,         /* 82 */
        CURLE_SSL_ISSUER_ERROR,        /* 83 */
        CURLE_FTP_PRET_FAILED,         /* 84 */
        CURLE_RTSP_CSEQ_ERROR,         /* 85 */
        CURLE_RTSP_SESSION_ERROR,      /* 86 */
        CURLE_FTP_BAD_FILE_LIST,       /* 87 */
        CURLE_CHUNK_FAILED,            /* 88 */
        CURLE_NO_CONNECTION_AVAILABLE, /* 89 */
        CURLE_SSL_PINNEDPUBKEYNOTMATCH,/* 90 */
        CURLE_SSL_INVALIDCERTSTATUS,   /* 91 */
        CURLE_HTTP2_STREAM,            /* 92 */
        CURLE_RECURSIVE_API_CALL,      /* 93 */
        CURLE_AUTH_ERROR,              /* 94 */
        CURLE_HTTP3,                   /* 95 */
        CURLE_QUIC_CONNECT_ERROR,      /* 96 */
        CURLE_PROXY,                   /* 97 */
        CURLE_SSL_CLIENTCERT,          /* 98 */
        CURLE_UNRECOVERABLE_POLL,      /* 99 */
        CURLE_TOO_LARGE,               /* 100 */
        CURLE_ECH_REQUIRED,            /* 101 */
        CURL_LAST /* never use! */
    }

    public enum CURLMcode
    {
        CURLM_CALL_MULTI_PERFORM = -1,
        CURLM_OK,
        CURLM_BAD_HANDLE,
        CURLM_BAD_EASY_HANDLE,
        CURLM_OUT_OF_MEMORY,
        CURLM_INTERNAL_ERROR,
        CURLM_BAD_SOCKET,
        CURLM_UNKNOWN_OPTION,
        CURLM_ADDED_ALREADY,
        CURLM_RECURSIVE_API_CALL,
        CURLM_WAKEUP_FAILURE,
        CURLM_BAD_FUNCTION_ARGUMENT,
        CURLM_ABORTED_BY_CALLBACK,
        CURLM_UNRECOVERABLE_POLL,
        CURLM_LAST
    }

    public enum CURLINFO
    {
        CURLINFO_NONE = 0,
        CURLINFO_EFFECTIVE_URL   = CURLDefines.CURLINFO_STRING + 1,
        CURLINFO_RESPONSE_CODE   = CURLDefines.CURLINFO_LONG + 2,
        CURLINFO_TOTAL_TIME      = CURLDefines.CURLINFO_DOUBLE + 3,
        CURLINFO_NAMELOOKUP_TIME = CURLDefines.CURLINFO_DOUBLE + 4,
        CURLINFO_CONNECT_TIME    = CURLDefines.CURLINFO_DOUBLE + 5,
        CURLINFO_SIZE_DOWNLOAD_T = CURLDefines.CURLINFO_OFF_T + 8,
        CURLINFO_SPEED_DOWNLOAD_T = CURLDefines.CURLINFO_OFF_T + 9,
        CURLINFO_SIZE_UPLOAD_T   = CURLDefines.CURLINFO_OFF_T + 7,
        CURLINFO_CONTENT_LENGTH_DOWNLOAD_T = CURLDefines.CURLINFO_OFF_T + 15,
        CURLINFO_CONTENT_TYPE    = CURLDefines.CURLINFO_STRING + 18,
        CURLINFO_REDIRECT_URL    = CURLDefines.CURLINFO_STRING + 31,
        CURLINFO_PRIMARY_IP      = CURLDefines.CURLINFO_STRING + 32,
        CURLINFO_HTTP_VERSION    = CURLDefines.CURLINFO_LONG + 46,
        CURLINFO_TOTAL_TIME_T    = CURLDefines.CURLINFO_OFF_T + 50,
        CURLINFO_PRIVATE         = CURLDefines.CURLINFO_STRING + 21,

        // --- Tier 1 additions (values verified against curl 8.21.0 curl.h) ----
        // All covered by the existing getinfo_long / _double / _pointer entry
        // points (OFF_T infos read via getinfo_long, which branches on the type
        // mask natively).

        // Upload counters
        CURLINFO_SPEED_UPLOAD_T          = CURLDefines.CURLINFO_OFF_T + 10,
        CURLINFO_CONTENT_LENGTH_UPLOAD_T = CURLDefines.CURLINFO_OFF_T + 16,

        // Timing breakdown (double = seconds; *_T = microseconds as OFF_T)
        CURLINFO_PRETRANSFER_TIME    = CURLDefines.CURLINFO_DOUBLE + 6,
        CURLINFO_STARTTRANSFER_TIME  = CURLDefines.CURLINFO_DOUBLE + 17,
        CURLINFO_APPCONNECT_TIME     = CURLDefines.CURLINFO_DOUBLE + 33,  /* TLS handshake done */
        CURLINFO_REDIRECT_TIME       = CURLDefines.CURLINFO_DOUBLE + 19,
        CURLINFO_NAMELOOKUP_TIME_T   = CURLDefines.CURLINFO_OFF_T + 51,
        CURLINFO_CONNECT_TIME_T      = CURLDefines.CURLINFO_OFF_T + 52,
        CURLINFO_PRETRANSFER_TIME_T  = CURLDefines.CURLINFO_OFF_T + 53,
        CURLINFO_STARTTRANSFER_TIME_T = CURLDefines.CURLINFO_OFF_T + 54,
        CURLINFO_APPCONNECT_TIME_T   = CURLDefines.CURLINFO_OFF_T + 56,

        // Connection / diagnostics
        CURLINFO_REDIRECT_COUNT   = CURLDefines.CURLINFO_LONG + 20,
        CURLINFO_NUM_CONNECTS     = CURLDefines.CURLINFO_LONG + 26,
        CURLINFO_OS_ERRNO         = CURLDefines.CURLINFO_LONG + 25,
        CURLINFO_SSL_VERIFYRESULT = CURLDefines.CURLINFO_LONG + 13,
        CURLINFO_PRIMARY_PORT     = CURLDefines.CURLINFO_LONG + 40,
        CURLINFO_LOCAL_PORT       = CURLDefines.CURLINFO_LONG + 42,
        CURLINFO_LOCAL_IP         = CURLDefines.CURLINFO_STRING + 41,
        CURLINFO_SCHEME           = CURLDefines.CURLINFO_STRING + 49,
    }

    public enum CURLMSG
    {
        CURLMSG_NONE,
        CURLMSG_DONE, /* transfer complete; result carries the CURLcode */
        CURLMSG_LAST
    }

    // --- share API enums (curl 8.21.0) --------------------------------------
    public enum CURLSHcode
    {
        CURLSHE_OK,
        CURLSHE_BAD_OPTION,   /* 1 */
        CURLSHE_IN_USE,       /* 2 */
        CURLSHE_INVALID,      /* 3 */
        CURLSHE_NOMEM,        /* 4 */
        CURLSHE_NOT_BUILT_IN, /* 5 */
        CURLSHE_LAST
    }

    public enum CURLSHoption
    {
        CURLSHOPT_NONE,
        CURLSHOPT_SHARE,      /* CURL_LOCK_DATA_* to start sharing */
        CURLSHOPT_UNSHARE,
        CURLSHOPT_LOCKFUNC,
        CURLSHOPT_UNLOCKFUNC,
        CURLSHOPT_USERDATA,
        CURLSHOPT_LAST
    }

    /// <summary>Values for CURLSHOPT_SHARE / CURLSHOPT_UNSHARE.</summary>
    public enum CURLlockData
    {
        CURL_LOCK_DATA_NONE = 0,
        CURL_LOCK_DATA_SHARE,
        CURL_LOCK_DATA_COOKIE,
        CURL_LOCK_DATA_DNS,
        CURL_LOCK_DATA_SSL_SESSION,
        CURL_LOCK_DATA_CONNECT,
        CURL_LOCK_DATA_PSL,
        CURL_LOCK_DATA_HSTS,
        CURL_LOCK_DATA_LAST
    }

    // --- URL API enums (urlapi.h) -------------------------------------------
    public enum CURLUPart
    {
        CURLUPART_URL,
        CURLUPART_SCHEME,
        CURLUPART_USER,
        CURLUPART_PASSWORD,
        CURLUPART_OPTIONS,
        CURLUPART_HOST,
        CURLUPART_PORT,
        CURLUPART_PATH,
        CURLUPART_QUERY,
        CURLUPART_FRAGMENT,
        CURLUPART_ZONEID
    }

    public enum CURLUcode
    {
        CURLUE_OK,
        CURLUE_BAD_HANDLE, CURLUE_BAD_PARTPOINTER, CURLUE_MALFORMED_INPUT,
        CURLUE_BAD_PORT_NUMBER, CURLUE_UNSUPPORTED_SCHEME, CURLUE_URLDECODE,
        CURLUE_OUT_OF_MEMORY, CURLUE_USER_NOT_ALLOWED, CURLUE_UNKNOWN_PART,
        CURLUE_NO_SCHEME, CURLUE_NO_USER, CURLUE_NO_PASSWORD, CURLUE_NO_OPTIONS,
        CURLUE_NO_HOST, CURLUE_NO_PORT, CURLUE_NO_QUERY, CURLUE_NO_FRAGMENT,
        CURLUE_NO_ZONEID, CURLUE_BAD_FILE_URL, CURLUE_BAD_FRAGMENT,
        CURLUE_BAD_HOSTNAME, CURLUE_BAD_IPV6, CURLUE_BAD_LOGIN, CURLUE_BAD_PASSWORD,
        CURLUE_BAD_PATH, CURLUE_BAD_QUERY, CURLUE_BAD_SCHEME, CURLUE_BAD_SLASHES,
        CURLUE_BAD_USER, CURLUE_LACKS_IDN, CURLUE_TOO_LARGE, CURLUE_LAST
    }

    public enum CURLHcode
    {
        CURLHE_OK,
        CURLHE_BADINDEX,      /* 1 */
        CURLHE_MISSING,       /* 2 */
        CURLHE_NOHEADERS,     /* 3 */
        CURLHE_NOREQUEST,     /* 4 */
        CURLHE_OUT_OF_MEMORY, /* 5 */
        CURLHE_BAD_ARGUMENT,  /* 6 */
        CURLHE_NOT_BUILT_IN   /* 7 */
    }

    /// <summary>Constants for the URL API flags, header origins, pause, ws, version bits.</summary>
    public static class CURLExtras
    {
        // curl_url_get / _set flags
        public const uint CURLU_DEFAULT_PORT = 1u << 0;
        public const uint CURLU_NO_DEFAULT_PORT = 1u << 1;
        public const uint CURLU_DEFAULT_SCHEME = 1u << 2;
        public const uint CURLU_NON_SUPPORT_SCHEME = 1u << 3;
        public const uint CURLU_PATH_AS_IS = 1u << 4;
        public const uint CURLU_URLDECODE = 1u << 6;
        public const uint CURLU_URLENCODE = 1u << 7;
        public const uint CURLU_APPENDQUERY = 1u << 8;
        public const uint CURLU_GUESS_SCHEME = 1u << 9;
        public const uint CURLU_PUNYCODE = 1u << 12;

        // curl_easy_header origin bits
        public const uint CURLH_HEADER = 1u << 0;
        public const uint CURLH_TRAILER = 1u << 1;
        public const uint CURLH_CONNECT = 1u << 2;
        public const uint CURLH_PSEUDO = 1u << 4;

        // curl_easy_pause action bits
        public const int CURLPAUSE_RECV = 1 << 0;
        public const int CURLPAUSE_SEND = 1 << 2;
        public const int CURLPAUSE_ALL = CURLPAUSE_RECV | CURLPAUSE_SEND;
        public const int CURLPAUSE_CONT = 0;

        // curl_ws_send / frame flags (CURLWS_*)
        public const uint CURLWS_TEXT = 1u << 0;
        public const uint CURLWS_BINARY = 1u << 1;
        public const uint CURLWS_CONT = 1u << 2;
        public const uint CURLWS_CLOSE = 1u << 3;
        public const uint CURLWS_PING = 1u << 4;
        public const uint CURLWS_OFFSET = 1u << 5;
        public const uint CURLWS_PONG = 1u << 6;

        // curl_version_info features bitmask (CURL_VERSION_*)
        public const int CURL_VERSION_IPV6 = 1 << 0;
        public const int CURL_VERSION_SSL = 1 << 2;
        public const int CURL_VERSION_ASYNCHDNS = 1 << 7;
        public const int CURL_VERSION_HTTP2 = 1 << 16;
        public const int CURL_VERSION_BROTLI = 1 << 23;
        public const int CURL_VERSION_ALTSVC = 1 << 24;
        public const int CURL_VERSION_HTTP3 = 1 << 25;
        public const int CURL_VERSION_ZSTD = 1 << 26;
        public const int CURL_VERSION_HSTS = 1 << 28;
    }

    /// <summary>curl_infotype — the kind of data passed to a debug callback.</summary>
    public enum CURLINFOTYPE
    {
        CURLINFO_TEXT = 0,
        CURLINFO_HEADER_IN,    /* 1 */
        CURLINFO_HEADER_OUT,   /* 2 */
        CURLINFO_DATA_IN,      /* 3 */
        CURLINFO_DATA_OUT,     /* 4 */
        CURLINFO_SSL_DATA_IN,  /* 5 */
        CURLINFO_SSL_DATA_OUT, /* 6 */
    }

    public static class CURLWaitPoll
    {
        public const short CURL_WAIT_POLLIN  = 0x0001;
        public const short CURL_WAIT_POLLPRI = 0x0002;
        public const short CURL_WAIT_POLLOUT = 0x0004;
    }

    /// <summary>
    /// Download data callback. size*nmemb bytes are available at <paramref name="content"/>.
    /// Return the number of bytes consumed; returning anything other than size*nmemb
    /// signals an error to curl. MUST be a static method decorated with
    /// [MonoPInvokeCallback(typeof(CurlwWriteDataDelegate))] for IL2CPP/AOT targets.
    /// </summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate UIntPtr CurlwWriteDataDelegate(IntPtr content, UIntPtr size, UIntPtr nmemb, IntPtr userdata);

    /// <summary>
    /// Header callback (CURLOPT_HEADERFUNCTION). Same ABI as the write callback:
    /// one header line per invocation. Return size*nmemb to continue.
    /// MUST be static + [MonoPInvokeCallback(typeof(CurlwWriteDataDelegate))].
    /// </summary>
    // (Header callback reuses CurlwWriteDataDelegate — identical native signature.)

    /// <summary>
    /// Progress callback (CURLOPT_XFERINFOFUNCTION). Return non-zero to abort the
    /// transfer. MUST be static + [MonoPInvokeCallback(typeof(CurlwXferInfoDelegate))].
    /// </summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate int CurlwXferInfoDelegate(IntPtr clientp, long dltotal, long dlnow, long ultotal, long ulnow);

    /// <summary>
    /// Debug/verbose callback (CURLOPT_DEBUGFUNCTION). <paramref name="data"/> holds
    /// <paramref name="size"/> bytes of the given <paramref name="type"/> (e.g. TEXT
    /// lines contain "ALPN: server accepted h2"). Return 0.
    /// MUST be static + [MonoPInvokeCallback(typeof(CurlwDebugDelegate))].
    /// </summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate int CurlwDebugDelegate(IntPtr handle, CURLINFOTYPE type, IntPtr data, UIntPtr size, IntPtr userptr);

    /// <summary>
    /// Managed open/close socket callback. Matches native curlw_socket_managed_cb.
    /// For open: called first with sockfd == -1 (return non-zero to allow the
    /// socket, zero to refuse); then again with the created fd. For close: called
    /// with the fd to be closed (return non-zero if you took ownership of closing it).
    /// MUST be static + [MonoPInvokeCallback] on IL2CPP/AOT.
    /// </summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    public delegate int CurlwSocketManagedDelegate(IntPtr sockfd, IntPtr userptr);

    /// <summary>
    /// Thin 1:1 binding of the native curlw C ABI (see modules/curlw/mod.rs). All entry points
    /// use cdecl to match NATIVEBRIDGE_CALL.
    /// </summary>
    public static class CurlwDLL
    {
#if (UNITY_IOS || UNITY_TVOS) && !UNITY_EDITOR
        public const string LIBNAME = "__Internal";
#else
        public const string LIBNAME = "NativeBridge";
#endif

        // --- UTF-8 string marshalling ----------------------------------------
        // byte[] parameters are copied verbatim by the P/Invoke marshaler (no
        // code page conversion), so a NUL-terminated UTF-8 buffer is what
        // libcurl sees. Keep every string-taking DllImport below on byte[] and
        // expose only string overloads that go through these helpers.
        private static byte[] Utf8Bytes(string s)
        {
            byte[] buf = new byte[Encoding.UTF8.GetByteCount(s) + 1];
            Encoding.UTF8.GetBytes(s, 0, s.Length, buf, 0); // trailing byte stays 0
            return buf;
        }

        // Reads a NUL-terminated UTF-8 C string. unsafe: pointer scan for the
        // NUL terminator avoids one Marshal.ReadByte call per byte.
        private static unsafe string PtrToStringUtf8(IntPtr p)
        {
            if (p == IntPtr.Zero) return null;
            byte* b = (byte*)p;
            int len = 0;
            while (b[len] != 0) len++;
            if (len == 0) return string.Empty;
            return Encoding.UTF8.GetString(b, len);
        }

        // Reads a UTF-8 C buffer of exactly byteLen bytes (may embed NULs).
        private static string PtrToStringUtf8(IntPtr p, int byteLen)
        {
            if (p == IntPtr.Zero || byteLen < 0) return null;
            if (byteLen == 0) return string.Empty;
            byte[] buf = new byte[byteLen];
            Marshal.Copy(p, buf, 0, byteLen);
            return Encoding.UTF8.GetString(buf);
        }

        // --- version / ABI ---------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_abi_version();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_version_imp")]
        private static extern IntPtr curlw_version_imp();

        public static string curlw_version()
        {
            return PtrToStringUtf8(curlw_version_imp());
        }

        // NativeBridge library-level version string.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "nativebridge_version")]
        private static extern IntPtr nativebridge_version_imp();

        public static string nativebridge_version()
        {
            return PtrToStringUtf8(nativebridge_version_imp());
        }

        // --- raw socket helpers ----------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_create_socket(int af, int type, int protocol);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_shutdown_socket(IntPtr sockfd);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_close_socket(IntPtr sockfd);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_errno();

        // --- global init / cleanup -------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_global_init(int flags, uint max_fd_set = 32);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_global_cleanup();

        // --- fd_set pool + select --------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_socket_allocfds();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_socket_freefds(IntPtr pfds);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_socket_zerofds(IntPtr pfds);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_socket_select(int nfds, IntPtr readfds, IntPtr writefds,
                                                      IntPtr exceptfds, ulong microseconds);

        // --- easy API --------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLH curlw_easy_init();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_perform(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_cleanup")]
        private static extern void curlw_easy_cleanup_imp(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_reset")]
        private static extern void curlw_easy_reset_imp(CURLH handle);

        // Managed wrappers release the per-handle callback strong references (set
        // via the delegate curlw_easy_setopt_pointer overload) so they can be GC'd
        // once curl no longer holds them.
        public static void curlw_easy_cleanup(CURLH handle)
        {
            curlw_easy_cleanup_imp(handle);
            ReleaseHandleCallbacks(handle);
        }

        public static void curlw_easy_reset(CURLH handle)
        {
            curlw_easy_reset_imp(handle);
            ReleaseHandleCallbacks(handle);
        }

        private static void ReleaseHandleCallbacks(IntPtr handle)
        {
            lock (s_cbLock)
            {
                s_handleCallbacks.Remove(handle);
            }
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_strerror_imp")]
        private static extern IntPtr curlw_easy_strerror_imp(CURLcode error);

        public static string curlw_easy_strerror(CURLcode error)
        {
            return PtrToStringUtf8(curlw_easy_strerror_imp(error));
        }

        // setopt: typed variants (curl_easy_setopt is variadic; do NOT P/Invoke it directly).
        // Native code validates the CURLOPTTYPE class of the option; within OBJECTPOINT
        // (string vs opaque pointer) discrimination relies on the C# type system.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_setopt_int(CURLH handle, CURLoption option, int optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_setopt_long(CURLH handle, CURLoption option, long optval);

        // For CURLOPTTYPE_OFF_T options only.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_setopt_offt(CURLH handle, CURLoption option, long optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_setopt_pointer(CURLH handle, CURLoption option, IntPtr optval);

        // Delegate overload: curl stores this function pointer on the easy handle
        // and invokes it during perform(). The managed delegate MUST stay alive
        // until the handle is cleaned up, else GC frees the thunk and the next
        // callback jumps into freed memory. This overload registers the delegate
        // in s_handleCallbacks; call curlw_easy_cleanup/reset (the managed
        // wrappers below) to release it.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_setopt_pointer")]
        private static extern CURLcode curlw_easy_setopt_pointer_cb_imp(CURLH handle, CURLoption option, CurlwWriteDataDelegate optval);

        // Per-handle strong references to callback delegates (write/read/...).
        private static readonly Dictionary<IntPtr, List<Delegate>>
            s_handleCallbacks = new Dictionary<IntPtr, List<Delegate>>();
        private static readonly object s_cbLock = new object();

        public static CURLcode curlw_easy_setopt_pointer(CURLH handle, CURLoption option, CurlwWriteDataDelegate optval)
        {
            KeepAliveCallback(handle, optval);
            return curlw_easy_setopt_pointer_cb_imp(handle, option, optval);
        }

        // --- Tier 2 callback overloads (reuse the pointer setopt entry point) --
        // Each pins the managed delegate in s_handleCallbacks so the JIT thunk
        // survives until curlw_easy_cleanup/reset. Use CURLOPT_HEADERFUNCTION with
        // the write-callback overload above (identical native signature); the two
        // overloads below cover the progress and debug callbacks.

        private static void KeepAliveCallback(IntPtr handle, Delegate cb)
        {
            lock (s_cbLock)
            {
                if (!s_handleCallbacks.TryGetValue(handle, out var list))
                {
                    list = new List<Delegate>();
                    s_handleCallbacks[handle] = list;
                }
                list.Add(cb);
            }
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_setopt_pointer")]
        private static extern CURLcode curlw_easy_setopt_pointer_xferinfo_imp(CURLH handle, CURLoption option, CurlwXferInfoDelegate optval);

        public static CURLcode curlw_easy_setopt_pointer(CURLH handle, CURLoption option, CurlwXferInfoDelegate optval)
        {
            KeepAliveCallback(handle, optval);
            return curlw_easy_setopt_pointer_xferinfo_imp(handle, option, optval);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_setopt_pointer")]
        private static extern CURLcode curlw_easy_setopt_pointer_debug_imp(CURLH handle, CURLoption option, CurlwDebugDelegate optval);

        public static CURLcode curlw_easy_setopt_pointer(CURLH handle, CURLoption option, CurlwDebugDelegate optval)
        {
            KeepAliveCallback(handle, optval);
            return curlw_easy_setopt_pointer_debug_imp(handle, option, optval);
        }

        // --- Tier 3: BLOB setopt (CURLOPT_*_BLOB) -----------------------------
        // Native marshals (data, len, flags) into a struct curl_blob. Pass
        // copy=true (CURL_BLOB_COPY) so curl owns a copy and the managed buffer
        // need not outlive the call.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        private static extern CURLcode curlw_easy_setopt_blob(CURLH handle, CURLoption option,
                                                              IntPtr data, UIntPtr len, uint flags);

        /// <summary>
        /// Set a BLOB option (e.g. CURLOPT_CAINFO_BLOB) from a managed byte[]. The
        /// bytes are copied into libcurl (CURL_BLOB_COPY), so <paramref name="data"/>
        /// need not be pinned after this call returns.
        /// </summary>
        public static CURLcode curlw_easy_setopt_blob(CURLH handle, CURLoption option, byte[] data)
        {
            if (data == null) return CURLcode.CURLE_BAD_FUNCTION_ARGUMENT;
            GCHandle pin = GCHandle.Alloc(data, GCHandleType.Pinned);
            try
            {
                const uint CURL_BLOB_COPY = 1;
                return curlw_easy_setopt_blob(handle, option, pin.AddrOfPinnedObject(),
                                              (UIntPtr)data.Length, CURL_BLOB_COPY);
            }
            finally
            {
                pin.Free();
            }
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_setopt_string")]
        private static extern CURLcode curlw_easy_setopt_string_imp(CURLH handle, CURLoption option, byte[] optval);

        // UTF-8 safe (see Utf8Bytes). Passing null resets the option to default.
        public static CURLcode curlw_easy_setopt_string(CURLH handle, CURLoption option, string optval)
        {
            return curlw_easy_setopt_string_imp(handle, option, optval == null ? null : Utf8Bytes(optval));
        }

        // getinfo: typed variants. Native code validates the CURLINFO type mask.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_getinfo_int(CURLH handle, CURLINFO info, out int outval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_getinfo_long(CURLH handle, CURLINFO info, out long outval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_getinfo_double(CURLH handle, CURLINFO info, out double outval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_getinfo_pointer(CURLH handle, CURLINFO info, out IntPtr outval);

        // --- open/close socket callbacks -------------------------------------
        // The native side stores these delegates in process-global function
        // pointers, so the managed delegates MUST outlive every call. The public
        // wrappers below keep static strong references to prevent GC from
        // collecting the thunk (which would crash the next socket callback).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_set_opensocket_global_cb")]
        private static extern void curlw_easy_set_opensocket_global_cb_imp(CurlwSocketManagedDelegate cb);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_set_closesocket_global_cb")]
        private static extern void curlw_easy_set_closesocket_global_cb_imp(CurlwSocketManagedDelegate cb);

        // Static strong references keeping the global socket delegates alive for
        // the lifetime of the process (matching the native global pointers).
        private static CurlwSocketManagedDelegate s_openSocketCb;
        private static CurlwSocketManagedDelegate s_closeSocketCb;

        public static void curlw_easy_set_opensocket_global_cb(CurlwSocketManagedDelegate cb)
        {
            s_openSocketCb = cb; // keep alive across GC
            curlw_easy_set_opensocket_global_cb_imp(cb);
        }

        public static void curlw_easy_set_closesocket_global_cb(CurlwSocketManagedDelegate cb)
        {
            s_closeSocketCb = cb; // keep alive across GC
            curlw_easy_set_closesocket_global_cb_imp(cb);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_set_opensocket_cb(CURLH handle, IntPtr userdata);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_easy_clear_opensocket_cb(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_set_closesocket_cb(CURLH handle, IntPtr userdata);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_easy_clear_closesocket_cb(CURLH handle);

        // --- multi API -------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMH curlw_multi_init();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_cleanup(CURLMH multi_handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_add_handle(CURLMH multi_handle, CURLH easy_handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_remove_handle(CURLMH multi_handle, CURLH easy_handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_perform(CURLMH multi_handle, out int running_handles);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_fdset(CURLMH multi_handle, IntPtr read_fds,
                                                         IntPtr write_fds, IntPtr exc_fds, out int max_fd);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_timeout(CURLMH multi_handle, out long milliseconds);

        // --- Tier 3: event-driven driving (curl_multi_poll / wait / wakeup) ---
        // Prefer poll() over the fdset+select loop: it blocks until a socket is
        // ready (up to timeout_ms) instead of spinning. Pass extra_fds=IntPtr.Zero,
        // extra_nfds=0 when you have no extra descriptors. curlw_multi_wakeup can
        // be called from another thread to break a blocking poll early.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_poll(CURLMH multi_handle, IntPtr extra_fds,
                                                        uint extra_nfds, int timeout_ms, out int numfds);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_wait(CURLMH multi_handle, IntPtr extra_fds,
                                                        uint extra_nfds, int timeout_ms, out int numfds);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_wakeup(CURLMH multi_handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_multi_strerror_imp")]
        private static extern IntPtr curlw_multi_strerror_imp(CURLMcode error);

        public static string curlw_multi_strerror(CURLMcode error)
        {
            return PtrToStringUtf8(curlw_multi_strerror_imp(error));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_setopt_int(CURLMH multi_handle, int option, int optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_setopt_long(CURLMH multi_handle, int option, long optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_setopt_offt(CURLMH multi_handle, int option, long optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_setopt_pointer(CURLMH multi_handle, int option, IntPtr optval);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_multi_setopt_string")]
        private static extern CURLMcode curlw_multi_setopt_string_imp(CURLMH multi_handle, int option, byte[] optval);

        // UTF-8 safe (see Utf8Bytes). Passing null resets the option to default.
        public static CURLMcode curlw_multi_setopt_string(CURLMH multi_handle, int option, string optval)
        {
            return curlw_multi_setopt_string_imp(multi_handle, option, optval == null ? null : Utf8Bytes(optval));
        }

        // Returns a pointer into curl-owned memory. Read its fields with the
        // curlw_msg_* accessors below (no fragile struct-layout marshalling).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_multi_info_read(CURLMH multi_handle, out int msgs_in_queue);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_msg_get_msg(IntPtr msg);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLH curlw_msg_get_easy_handle(IntPtr msg);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_msg_get_result(IntPtr msg);

        // --- slist -----------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_slist_append")]
        private static extern IntPtr curlw_slist_append_imp(IntPtr list, byte[] value);

        // UTF-8 safe (see Utf8Bytes).
        public static IntPtr curlw_slist_append(IntPtr list, string value)
        {
            return curlw_slist_append_imp(list, value == null ? null : Utf8Bytes(value));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_slist_free_all(IntPtr list);

        // --- misc / memory ---------------------------------------------------
        // Frees memory allocated by libcurl (escape/url_get/get_handles results).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_free(IntPtr p);

        // --- easy: extra entry points ----------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLH curlw_easy_duphandle(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_pause(CURLH handle, int action);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_upkeep(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_recv(CURLH handle, [Out] byte[] buffer, UIntPtr buflen, out UIntPtr n);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_easy_send(CURLH handle, byte[] buffer, UIntPtr buflen, out UIntPtr n);

        // Returns a curl-allocated string; the managed wrapper copies it and frees
        // the native buffer for you. Input is encoded as UTF-8 (see Utf8Bytes).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_escape")]
        private static extern IntPtr curlw_easy_escape_imp(CURLH handle, byte[] s, int length);

        public static string curlw_easy_escape(CURLH handle, string s)
        {
            IntPtr p = curlw_easy_escape_imp(handle, s == null ? null : Utf8Bytes(s), 0); // 0 => strlen
            if (p == IntPtr.Zero) return null;
            string r = PtrToStringUtf8(p);
            curlw_free(p);
            return r;
        }

        // Input is encoded as UTF-8 (see Utf8Bytes); the decoded bytes are read
        // back as UTF-8 with the exact outlength (they may contain any bytes).
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_unescape")]
        private static extern IntPtr curlw_easy_unescape_imp(CURLH handle, byte[] s, int inlength, out int outlength);

        public static string curlw_easy_unescape(CURLH handle, string s)
        {
            IntPtr p = curlw_easy_unescape_imp(handle, s == null ? null : Utf8Bytes(s), 0, out int outlen);
            if (p == IntPtr.Zero) return null;
            string r = PtrToStringUtf8(p, outlen);
            curlw_free(p);
            return r;
        }

        // --- version info (read via accessors) -------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_version_info();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_verinfo_features(IntPtr d);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern uint curlw_verinfo_version_num(IntPtr d);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_version")]
        private static extern IntPtr curlw_verinfo_version_imp(IntPtr d);
        public static string curlw_verinfo_version(IntPtr d) => PtrToStringUtf8(curlw_verinfo_version_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_ssl_version")]
        private static extern IntPtr curlw_verinfo_ssl_version_imp(IntPtr d);
        public static string curlw_verinfo_ssl_version(IntPtr d) => PtrToStringUtf8(curlw_verinfo_ssl_version_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_libz_version")]
        private static extern IntPtr curlw_verinfo_libz_version_imp(IntPtr d);
        public static string curlw_verinfo_libz_version(IntPtr d) => PtrToStringUtf8(curlw_verinfo_libz_version_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_nghttp2_version")]
        private static extern IntPtr curlw_verinfo_nghttp2_version_imp(IntPtr d);
        public static string curlw_verinfo_nghttp2_version(IntPtr d) => PtrToStringUtf8(curlw_verinfo_nghttp2_version_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_quic_version")]
        private static extern IntPtr curlw_verinfo_quic_version_imp(IntPtr d);
        public static string curlw_verinfo_quic_version(IntPtr d) => PtrToStringUtf8(curlw_verinfo_quic_version_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_cainfo")]
        private static extern IntPtr curlw_verinfo_cainfo_imp(IntPtr d);
        public static string curlw_verinfo_cainfo(IntPtr d) => PtrToStringUtf8(curlw_verinfo_cainfo_imp(d));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_verinfo_capath")]
        private static extern IntPtr curlw_verinfo_capath_imp(IntPtr d);
        public static string curlw_verinfo_capath(IntPtr d) => PtrToStringUtf8(curlw_verinfo_capath_imp(d));

        // --- header API ------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_easy_header")]
        private static extern CURLHcode curlw_easy_header_imp(CURLH handle, byte[] name, UIntPtr nameindex,
                                                              uint origin, int request, out IntPtr hout);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLHcode curlw_easy_header(CURLH handle, string name, UIntPtr nameindex,
                                                  uint origin, int request, out IntPtr hout)
        {
            return curlw_easy_header_imp(handle, name == null ? null : Utf8Bytes(name),
                                         nameindex, origin, request, out hout);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_easy_nextheader(CURLH handle, uint origin, int request, IntPtr prev);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_header_name")]
        private static extern IntPtr curlw_header_name_imp(IntPtr h);
        public static string curlw_header_name(IntPtr h) => PtrToStringUtf8(curlw_header_name_imp(h));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_header_value")]
        private static extern IntPtr curlw_header_value_imp(IntPtr h);
        public static string curlw_header_value(IntPtr h) => PtrToStringUtf8(curlw_header_value_imp(h));

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern UIntPtr curlw_header_amount(IntPtr h);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern UIntPtr curlw_header_index(IntPtr h);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern uint curlw_header_origin(IntPtr h);

        // --- share API -------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_share_init();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLSHcode curlw_share_cleanup(IntPtr share);

        // Only CURLSHOPT_SHARE / CURLSHOPT_UNSHARE are valid here.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLSHcode curlw_share_setopt_int(IntPtr share, CURLSHoption option, int value);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLSHcode curlw_share_enable_default_locks(IntPtr share);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_share_strerror_imp")]
        private static extern IntPtr curlw_share_strerror_imp(CURLSHcode error);
        public static string curlw_share_strerror(CURLSHcode error) => PtrToStringUtf8(curlw_share_strerror_imp(error));

        // --- MIME ------------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_mime_init(CURLH easy);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_mime_free(IntPtr mime);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_mime_addpart(IntPtr mime);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_mime_name")]
        private static extern CURLcode curlw_mime_name_imp(IntPtr part, byte[] name);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLcode curlw_mime_name(IntPtr part, string name)
        {
            return curlw_mime_name_imp(part, name == null ? null : Utf8Bytes(name));
        }

        // Pass the exact byte length as datasize. Do NOT pass UIntPtr.MaxValue
        // (curl's CURL_ZERO_TERMINATED): with a byte[] the marshaller does not
        // append a NUL, so curl's strlen() would read past the array. For NUL-
        // terminated text use curlw_mime_data with the real length instead.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_mime_data(IntPtr part, byte[] data, UIntPtr datasize);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_mime_filedata")]
        private static extern CURLcode curlw_mime_filedata_imp(IntPtr part, byte[] filename);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLcode curlw_mime_filedata(IntPtr part, string filename)
        {
            return curlw_mime_filedata_imp(part, filename == null ? null : Utf8Bytes(filename));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_mime_filename")]
        private static extern CURLcode curlw_mime_filename_imp(IntPtr part, byte[] filename);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLcode curlw_mime_filename(IntPtr part, string filename)
        {
            return curlw_mime_filename_imp(part, filename == null ? null : Utf8Bytes(filename));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_mime_type")]
        private static extern CURLcode curlw_mime_type_imp(IntPtr part, byte[] mimetype);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLcode curlw_mime_type(IntPtr part, string mimetype)
        {
            return curlw_mime_type_imp(part, mimetype == null ? null : Utf8Bytes(mimetype));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_mime_encoder")]
        private static extern CURLcode curlw_mime_encoder_imp(IntPtr part, byte[] encoding);

        // UTF-8 safe (see Utf8Bytes).
        public static CURLcode curlw_mime_encoder(IntPtr part, string encoding)
        {
            return curlw_mime_encoder_imp(part, encoding == null ? null : Utf8Bytes(encoding));
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_mime_headers(IntPtr part, IntPtr headers, int take_ownership);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_mime_subparts(IntPtr part, IntPtr subparts);

        // --- URL API ---------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_url();

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern void curlw_url_cleanup(IntPtr handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_url_dup(IntPtr input);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_url_get")]
        private static extern CURLUcode curlw_url_get_imp(IntPtr handle, CURLUPart what, out IntPtr part, uint flags);

        public static CURLUcode curlw_url_get(IntPtr handle, CURLUPart what, out string part, uint flags)
        {
            CURLUcode ec = curlw_url_get_imp(handle, what, out IntPtr p, flags);
            part = (ec == CURLUcode.CURLUE_OK && p != IntPtr.Zero) ? PtrToStringUtf8(p) : null;
            if (p != IntPtr.Zero) curlw_free(p);
            return ec;
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_url_set")]
        private static extern CURLUcode curlw_url_set_imp(IntPtr handle, CURLUPart what, byte[] part, uint flags);

        // UTF-8 safe (see Utf8Bytes). Passing null clears the part.
        public static CURLUcode curlw_url_set(IntPtr handle, CURLUPart what, string part, uint flags)
        {
            return curlw_url_set_imp(handle, what, part == null ? null : Utf8Bytes(part), flags);
        }

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl, EntryPoint = "curlw_url_strerror_imp")]
        private static extern IntPtr curlw_url_strerror_imp(CURLUcode error);
        public static string curlw_url_strerror(CURLUcode error) => PtrToStringUtf8(curlw_url_strerror_imp(error));

        // --- WebSocket -------------------------------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_ws_recv(CURLH handle, [Out] byte[] buffer, UIntPtr buflen,
                                                    out UIntPtr recv, out IntPtr meta);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLcode curlw_ws_send(CURLH handle, byte[] buffer, UIntPtr buflen,
                                                    out UIntPtr sent, long fragsize, uint flags);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_ws_meta(CURLH handle);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern int curlw_wsframe_flags(IntPtr f);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern long curlw_wsframe_offset(IntPtr f);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern long curlw_wsframe_bytesleft(IntPtr f);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern UIntPtr curlw_wsframe_len(IntPtr f);

        // --- multi: event-driven extensions ----------------------------------
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_assign(CURLMH multi_handle, IntPtr sockfd, IntPtr sockp);

        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern CURLMcode curlw_multi_socket_action(CURLMH multi_handle, IntPtr s,
                                                                 int ev_bitmask, out int running_handles);

        // Returns a curl-allocated, NULL-terminated CURL* array; free with curlw_free.
        [DllImport(LIBNAME, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr curlw_multi_get_handles(CURLMH multi_handle);
    }
}
#endif // !UNITY_WEBGL
