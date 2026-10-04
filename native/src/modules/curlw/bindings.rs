use super::*;

pub type CURL = c_void;
pub type CURLM = c_void;
pub type CURLSH = c_void;
pub type CURLU = c_void;
pub type curl_mime = c_void;
pub type curl_mimepart = c_void;

// Windows SOCKET is UINT_PTR (pointer-width): 64-bit on x64. Must match the real
// winsock/curl type, otherwise fd_set / curl_waitfd / CURLINFO_SOCKET writes
// overflow the storage on 64-bit Windows.
#[cfg(windows)]
pub type curl_socket_t = usize;
#[cfg(not(windows))]
pub type curl_socket_t = c_int;

pub type curl_off_t = i64;
pub type CURLoption = c_int;
pub type CURLINFO = c_int;
pub type CURLcode = c_int;
pub type CURLMoption = c_int;
pub type CURLMcode = c_int;
pub type CURLSHoption = c_int;
pub type CURLSHcode = c_int;
pub type CURLversion = c_int;
pub type CURLMSG = c_uint;
pub type CURLHcode = c_int;
pub type CURLUPart = c_uint;
pub type CURLUcode = c_uint;
pub type curl_lock_data = c_int;
pub type curl_lock_access = c_int;
pub type curlioerr = c_int;
pub type curl_infotype = c_int;

pub const CURLE_OK: CURLcode = 0;
pub const CURLE_FAILED_INIT: CURLcode = 2;
pub const CURLE_OUT_OF_MEMORY: CURLcode = 27;
pub const CURLE_BAD_FUNCTION_ARGUMENT: CURLcode = 43;

pub const CURLM_OK: CURLMcode = 0;
pub const CURLM_BAD_FUNCTION_ARGUMENT: CURLMcode = 10;

pub const CURLSHE_OK: CURLSHcode = 0;
pub const CURLSHE_BAD_OPTION: CURLSHcode = 1;
pub const CURLSHE_INVALID: CURLSHcode = 3;
pub const CURLSHE_NOMEM: CURLSHcode = 4;

pub const CURLHE_OK: CURLHcode = 0;
pub const CURLHE_BAD_INDEX: CURLHcode = 1;
pub const CURLHE_MISSING_HEADER: CURLHcode = 2;
pub const CURLHE_NO_HEADER: CURLHcode = 3;
pub const CURLHE_NO_REQUEST: CURLHcode = 4;
pub const CURLHE_OUT_OF_MEMORY: CURLHcode = 5;
pub const CURLHE_BAD_ARGUMENT: CURLHcode = 6;
pub const CURLHE_NOT_BUILT_IN: CURLHcode = 7;

pub const CURLUE_OK: CURLUcode = 0;

pub const CURLVERSION_NOW: CURLversion = 11;
pub const CURLMSG_DONE: CURLMSG = 1;

pub const CURLUPART_URL: CURLUPart = 0;
pub const CURLUPART_SCHEME: CURLUPart = 1;
pub const CURLUPART_USER: CURLUPart = 2;
pub const CURLUPART_PASSWORD: CURLUPart = 3;
pub const CURLUPART_OPTIONS: CURLUPart = 4;
pub const CURLUPART_HOST: CURLUPart = 5;
pub const CURLUPART_PORT: CURLUPart = 6;
pub const CURLUPART_PATH: CURLUPart = 7;
pub const CURLUPART_QUERY: CURLUPart = 8;
pub const CURLUPART_FRAGMENT: CURLUPart = 9;
pub const CURLUPART_ZONEID: CURLUPart = 10;

pub const CURLU_DEFAULT_PORT: c_uint = 1 << 0;
pub const CURLU_NO_DEFAULT_PORT: c_uint = 1 << 1;
pub const CURLU_DEFAULT_SCHEME: c_uint = 1 << 2;
pub const CURLU_NON_SUPPORT_SCHEME: c_uint = 1 << 3;
pub const CURLU_PATH_AS_IS: c_uint = 1 << 4;
pub const CURLU_DISALLOW_USER: c_uint = 1 << 5;
pub const CURLU_URLDECODE: c_uint = 1 << 6;
pub const CURLU_URLENCODE: c_uint = 1 << 7;
pub const CURLU_APPENDQUERY: c_uint = 1 << 8;
pub const CURLU_GUESS_SCHEME: c_uint = 1 << 9;
pub const CURLU_NO_AUTHORITY: c_uint = 1 << 10;
pub const CURLU_ALLOW_SPACE: c_uint = 1 << 11;
pub const CURLU_PUNYCODE: c_uint = 1 << 12;
pub const CURLU_PUNY2IDN: c_uint = 1 << 13;

pub const CURLOPTTYPE_LONG: c_int = 0;
pub const CURLOPTTYPE_OBJECTPOINT: c_int = 10000;
pub const CURLOPTTYPE_FUNCTIONPOINT: c_int = 20000;
pub const CURLOPTTYPE_OFF_T: c_int = 30000;
pub const CURLOPTTYPE_BLOB: c_int = 40000;

pub const CURLINFO_TYPEMASK: c_int = 0xf00000;
pub const CURLINFO_STRING: c_int = 0x100000;
pub const CURLINFO_LONG: c_int = 0x200000;
pub const CURLINFO_DOUBLE: c_int = 0x300000;
pub const CURLINFO_SLIST: c_int = 0x400000;
pub const CURLINFO_SOCKET: c_int = 0x500000;
pub const CURLINFO_OFF_T: c_int = 0x600000;

macro_rules! opt_long {
    ($n:expr) => { CURLOPTTYPE_LONG + $n };
}
macro_rules! opt_obj {
    ($n:expr) => { CURLOPTTYPE_OBJECTPOINT + $n };
}
macro_rules! opt_func {
    ($n:expr) => { CURLOPTTYPE_FUNCTIONPOINT + $n };
}
macro_rules! opt_offt {
    ($n:expr) => { CURLOPTTYPE_OFF_T + $n };
}

pub const CURLOPT_WRITEDATA: CURLoption = opt_obj!(1);
pub const CURLOPT_URL: CURLoption = opt_obj!(2);
pub const CURLOPT_PORT: CURLoption = opt_long!(3);
pub const CURLOPT_PROXY: CURLoption = opt_obj!(4);
pub const CURLOPT_USERPWD: CURLoption = opt_obj!(5);
pub const CURLOPT_PROXYUSERPWD: CURLoption = opt_obj!(6);
pub const CURLOPT_RANGE: CURLoption = opt_obj!(7);
pub const CURLOPT_READDATA: CURLoption = opt_obj!(9);
pub const CURLOPT_ERRORBUFFER: CURLoption = opt_obj!(10);
pub const CURLOPT_WRITEFUNCTION: CURLoption = opt_func!(11);
pub const CURLOPT_READFUNCTION: CURLoption = opt_func!(12);
pub const CURLOPT_TIMEOUT: CURLoption = opt_long!(13);
pub const CURLOPT_TIMEOUT_MS: CURLoption = opt_long!(155);
pub const CURLOPT_INFILESIZE: CURLoption = opt_long!(14);
pub const CURLOPT_POSTFIELDS: CURLoption = opt_obj!(15);
pub const CURLOPT_REFERER: CURLoption = opt_obj!(16);
pub const CURLOPT_FTPPORT: CURLoption = opt_obj!(17);
pub const CURLOPT_USERAGENT: CURLoption = opt_obj!(18);
pub const CURLOPT_LOW_SPEED_LIMIT: CURLoption = opt_long!(19);
pub const CURLOPT_LOW_SPEED_TIME: CURLoption = opt_long!(20);
pub const CURLOPT_RESUME_FROM: CURLoption = opt_long!(21);
pub const CURLOPT_COOKIE: CURLoption = opt_obj!(22);
pub const CURLOPT_HTTPHEADER: CURLoption = opt_obj!(23);
pub const CURLOPT_HTTPPOST: CURLoption = opt_obj!(24);
pub const CURLOPT_SSLCERT: CURLoption = opt_obj!(25);
pub const CURLOPT_KEYPASSWD: CURLoption = opt_obj!(26);
pub const CURLOPT_CUSTOMREQUEST: CURLoption = opt_obj!(36);
pub const CURLOPT_VERBOSE: CURLoption = opt_long!(41);
pub const CURLOPT_HEADER: CURLoption = opt_long!(42);
pub const CURLOPT_NOBODY: CURLoption = opt_long!(44);
pub const CURLOPT_UPLOAD: CURLoption = opt_long!(46);
pub const CURLOPT_POST: CURLoption = opt_long!(47);
pub const CURLOPT_PUT: CURLoption = opt_long!(54);
pub const CURLOPT_POSTFIELDSIZE: CURLoption = opt_long!(60);
pub const CURLOPT_SSL_VERIFYPEER: CURLoption = opt_long!(64);
pub const CURLOPT_CAINFO: CURLoption = opt_obj!(65);
pub const CURLOPT_FOLLOWLOCATION: CURLoption = opt_long!(52);
pub const CURLOPT_PROXYPORT: CURLoption = opt_long!(59);
pub const CURLOPT_HTTPGET: CURLoption = opt_long!(80);
pub const CURLOPT_SSL_VERIFYHOST: CURLoption = opt_long!(81);
pub const CURLOPT_HTTP_VERSION: CURLoption = opt_long!(84);
pub const CURLOPT_NOSIGNAL: CURLoption = opt_long!(99);
pub const CURLOPT_PROXYTYPE: CURLoption = opt_long!(101);
pub const CURLOPT_SHARE: CURLoption = opt_obj!(100);
pub const CURLOPT_PRIVATE: CURLoption = opt_obj!(103);
pub const CURLOPT_ENCODING: CURLoption = opt_obj!(102);
pub const CURLOPT_ACCEPT_ENCODING: CURLoption = CURLOPT_ENCODING;
pub const CURLOPT_CAPATH: CURLoption = opt_obj!(97);
pub const CURLOPT_CONNECTTIMEOUT: CURLoption = opt_long!(78);
pub const CURLOPT_CONNECTTIMEOUT_MS: CURLoption = opt_long!(156);
pub const CURLOPT_SSLVERSION: CURLoption = opt_long!(32);
pub const CURLOPT_INTERFACE: CURLoption = opt_obj!(62);
pub const CURLOPT_DNS_CACHE_TIMEOUT: CURLoption = opt_long!(92);
pub const CURLOPT_DNS_SERVERS: CURLoption = opt_obj!(211);
pub const CURLOPT_DNS_LOCAL_IP4: CURLoption = opt_obj!(222);
pub const CURLOPT_DNS_LOCAL_IP6: CURLoption = opt_obj!(223);
pub const CURLOPT_RESOLVE: CURLoption = opt_obj!(203);
pub const CURLOPT_USE_SSL: CURLoption = opt_long!(119);
pub const CURLOPT_SSL_OPTIONS: CURLoption = opt_long!(216);
pub const CURLOPT_HTTPAUTH: CURLoption = opt_long!(107);
pub const CURLOPT_PROXYAUTH: CURLoption = opt_long!(111);
pub const CURLOPT_SSH_AUTH_TYPES: CURLoption = opt_long!(151);
pub const CURLOPT_PROTOCOLS: CURLoption = opt_long!(181);
pub const CURLOPT_REDIR_PROTOCOLS: CURLoption = opt_long!(182);
pub const CURLOPT_POSTREDIR: CURLoption = opt_long!(161);
pub const CURLOPT_SOCKS5_AUTH: CURLoption = opt_long!(267);
pub const CURLOPT_SOCKOPTDATA: CURLoption = opt_obj!(149);
pub const CURLOPT_OPENSOCKETFUNCTION: CURLoption = opt_func!(163);
pub const CURLOPT_OPENSOCKETDATA: CURLoption = opt_obj!(164);
pub const CURLOPT_CLOSESOCKETFUNCTION: CURLoption = opt_func!(208);
pub const CURLOPT_CLOSESOCKETDATA: CURLoption = opt_obj!(209);
pub const CURLOPT_HEADERDATA: CURLoption = opt_obj!(29);
pub const CURLOPT_HEADERFUNCTION: CURLoption = opt_func!(79);
pub const CURLOPT_COPYPOSTFIELDS: CURLoption = opt_obj!(165);
pub const CURLOPT_POSTFIELDSIZE_LARGE: CURLoption = opt_offt!(120);
pub const CURLOPT_INFILESIZE_LARGE: CURLoption = opt_offt!(115);
pub const CURLOPT_RESUME_FROM_LARGE: CURLoption = opt_offt!(116);
pub const CURLOPT_MAX_RECV_SPEED_LARGE: CURLoption = opt_offt!(146);
pub const CURLOPT_MAX_SEND_SPEED_LARGE: CURLoption = opt_offt!(145);
pub const CURLOPT_ALTSVC_CTRL: CURLoption = opt_long!(286);
pub const CURLOPT_MIMEPOST: CURLoption = opt_obj!(269);
pub const CURLOPT_INTERLEAVEDATA: CURLoption = opt_obj!(195);
pub const CURLOPT_INTERLEAVEFUNCTION: CURLoption = opt_func!(196);

pub const CURLINFO_RESPONSE_CODE: CURLINFO = CURLINFO_LONG + 0x2;
pub const CURLINFO_HTTP_VERSION: CURLINFO = CURLINFO_LONG + 46;
pub const CURLINFO_TOTAL_TIME: CURLINFO = CURLINFO_DOUBLE + 0x3;
pub const CURLINFO_NAMELOOKUP_TIME: CURLINFO = CURLINFO_DOUBLE + 0x4;
pub const CURLINFO_CONNECT_TIME: CURLINFO = CURLINFO_DOUBLE + 0x5;
pub const CURLINFO_APPCONNECT_TIME: CURLINFO = CURLINFO_DOUBLE + 33;
pub const CURLINFO_PRETRANSFER_TIME: CURLINFO = CURLINFO_DOUBLE + 6;
pub const CURLINFO_STARTTRANSFER_TIME: CURLINFO = CURLINFO_DOUBLE + 17;
pub const CURLINFO_REDIRECT_TIME: CURLINFO = CURLINFO_DOUBLE + 19;
pub const CURLINFO_REDIRECT_COUNT: CURLINFO = CURLINFO_LONG + 20;
pub const CURLINFO_REDIRECT_URL: CURLINFO = CURLINFO_STRING + 31;
pub const CURLINFO_EFFECTIVE_URL: CURLINFO = CURLINFO_STRING + 1;
pub const CURLINFO_CONTENT_TYPE: CURLINFO = CURLINFO_STRING + 18;
pub const CURLINFO_CONTENT_LENGTH_DOWNLOAD: CURLINFO = CURLINFO_DOUBLE + 15;
pub const CURLINFO_CONTENT_LENGTH_UPLOAD: CURLINFO = CURLINFO_DOUBLE + 16;
pub const CURLINFO_SIZE_DOWNLOAD: CURLINFO = CURLINFO_DOUBLE + 8;
pub const CURLINFO_SIZE_UPLOAD: CURLINFO = CURLINFO_DOUBLE + 7;
pub const CURLINFO_SPEED_DOWNLOAD: CURLINFO = CURLINFO_DOUBLE + 9;
pub const CURLINFO_SPEED_UPLOAD: CURLINFO = CURLINFO_DOUBLE + 10;
pub const CURLINFO_HEADER_SIZE: CURLINFO = CURLINFO_LONG + 11;
pub const CURLINFO_REQUEST_SIZE: CURLINFO = CURLINFO_LONG + 12;
pub const CURLINFO_SSL_VERIFYRESULT: CURLINFO = CURLINFO_LONG + 13;
pub const CURLINFO_FILETIME: CURLINFO = CURLINFO_LONG + 14;
pub const CURLINFO_FILETIME_T: CURLINFO = CURLINFO_OFF_T + 14;
pub const CURLINFO_HTTPAUTH_AVAIL: CURLINFO = CURLINFO_LONG + 23;
pub const CURLINFO_PROXYAUTH_AVAIL: CURLINFO = CURLINFO_LONG + 24;
pub const CURLINFO_OS_ERRNO: CURLINFO = CURLINFO_LONG + 25;
pub const CURLINFO_NUM_CONNECTS: CURLINFO = CURLINFO_LONG + 26;
pub const CURLINFO_PRIMARY_IP: CURLINFO = CURLINFO_STRING + 32;
pub const CURLINFO_PRIMARY_PORT: CURLINFO = CURLINFO_LONG + 40;
pub const CURLINFO_LOCAL_IP: CURLINFO = CURLINFO_STRING + 41;
pub const CURLINFO_LOCAL_PORT: CURLINFO = CURLINFO_LONG + 42;
pub const CURLINFO_COOKIELIST: CURLINFO = CURLINFO_SLIST + 28;
pub const CURLINFO_LASTSOCKET: CURLINFO = CURLINFO_LONG + 29;
pub const CURLINFO_ACTIVESOCKET: CURLINFO = CURLINFO_SOCKET + 44;
pub const CURLINFO_CERTINFO: CURLINFO = CURLINFO_SLIST + 34;
pub const CURLINFO_PRIVATE: CURLINFO = CURLINFO_STRING + 21;
pub const CURLINFO_RETRY_AFTER: CURLINFO = CURLINFO_OFF_T + 57;
pub const CURLINFO_HTTP_CONNECTCODE: CURLINFO = CURLINFO_LONG + 22;
pub const CURLINFO_PROTOCOL: CURLINFO = CURLINFO_LONG + 48;
pub const CURLINFO_SCHEME: CURLINFO = CURLINFO_STRING + 49;
pub const CURLINFO_APPCONNECT_TIME_T: CURLINFO = CURLINFO_OFF_T + 56;
pub const CURLINFO_CONNECT_TIME_T: CURLINFO = CURLINFO_OFF_T + 52;
pub const CURLINFO_NAMELOOKUP_TIME_T: CURLINFO = CURLINFO_OFF_T + 51;
pub const CURLINFO_PRETRANSFER_TIME_T: CURLINFO = CURLINFO_OFF_T + 53;
pub const CURLINFO_REDIRECT_TIME_T: CURLINFO = CURLINFO_OFF_T + 55;
pub const CURLINFO_STARTTRANSFER_TIME_T: CURLINFO = CURLINFO_OFF_T + 54;
pub const CURLINFO_TOTAL_TIME_T: CURLINFO = CURLINFO_OFF_T + 50;
pub const CURLINFO_SIZE_DOWNLOAD_T: CURLINFO = CURLINFO_OFF_T + 8;
pub const CURLINFO_SIZE_UPLOAD_T: CURLINFO = CURLINFO_OFF_T + 7;
pub const CURLINFO_SPEED_DOWNLOAD_T: CURLINFO = CURLINFO_OFF_T + 9;
pub const CURLINFO_SPEED_UPLOAD_T: CURLINFO = CURLINFO_OFF_T + 10;
pub const CURLINFO_CONTENT_LENGTH_DOWNLOAD_T: CURLINFO = CURLINFO_OFF_T + 15;
pub const CURLINFO_CONTENT_LENGTH_UPLOAD_T: CURLINFO = CURLINFO_OFF_T + 16;
pub const CURLINFO_EFFECTIVE_METHOD: CURLINFO = CURLINFO_STRING + 58;
pub const CURLINFO_XFER_ID: CURLINFO = CURLINFO_OFF_T + 63;
pub const CURLINFO_CONN_ID: CURLINFO = CURLINFO_OFF_T + 64;
pub const CURLINFO_TLS_SSL_PTR: CURLINFO = CURLINFO_SLIST + 45;

pub const CURLMOPT_SOCKETFUNCTION: CURLMoption = opt_func!(1);
pub const CURLMOPT_SOCKETDATA: CURLMoption = opt_obj!(2);
pub const CURLMOPT_PIPELINING: CURLMoption = opt_long!(3);
pub const CURLMOPT_TIMERFUNCTION: CURLMoption = opt_func!(4);
pub const CURLMOPT_TIMERDATA: CURLMoption = opt_obj!(5);
pub const CURLMOPT_MAXCONNECTS: CURLMoption = opt_long!(6);
pub const CURLMOPT_MAX_HOST_CONNECTIONS: CURLMoption = opt_long!(7);
pub const CURLMOPT_MAX_TOTAL_CONNECTIONS: CURLMoption = opt_long!(13);

pub const CURLSHOPT_SHARE: CURLSHoption = 1;
pub const CURLSHOPT_UNSHARE: CURLSHoption = 2;
pub const CURLSHOPT_LOCKFUNC: CURLSHoption = 3;
pub const CURLSHOPT_UNLOCKFUNC: CURLSHoption = 4;
pub const CURLSHOPT_USERDATA: CURLSHoption = 5;

pub const CURL_LOCK_DATA_NONE: curl_lock_data = 0;
pub const CURL_LOCK_DATA_SHARE: curl_lock_data = 1;
pub const CURL_LOCK_DATA_COOKIE: curl_lock_data = 2;
pub const CURL_LOCK_DATA_DNS: curl_lock_data = 3;
pub const CURL_LOCK_DATA_SSL_SESSION: curl_lock_data = 4;
pub const CURL_LOCK_DATA_CONNECT: curl_lock_data = 5;
pub const CURL_LOCK_DATA_PSL: curl_lock_data = 6;
pub const CURL_LOCK_DATA_HSTS: curl_lock_data = 7;
pub const CURL_LOCK_DATA_LAST: curl_lock_data = 8;

pub const CURL_LOCK_ACCESS_NONE: curl_lock_access = 0;
pub const CURL_LOCK_ACCESS_SHARED: curl_lock_access = 1;
pub const CURL_LOCK_ACCESS_SINGLE: curl_lock_access = 2;

pub const CURL_VERSION_IPV6: c_int = 1;
pub const CURL_VERSION_KERBEROS4: c_int = 2;
pub const CURL_VERSION_SSL: c_int = 4;
pub const CURL_VERSION_LIBZ: c_int = 8;
pub const CURL_VERSION_NTLM: c_int = 16;
pub const CURL_VERSION_GSSNEGOTIATE: c_int = 32;
pub const CURL_VERSION_DEBUG: c_int = 64;
pub const CURL_VERSION_ASYNCHDNS: c_int = 128;
pub const CURL_VERSION_SPNEGO: c_int = 256;
pub const CURL_VERSION_HTTP2: c_int = 65536;
pub const CURL_VERSION_BROTLI: c_int = 1 << 23;
pub const CURL_VERSION_HTTP3: c_int = 1 << 25;
pub const CURL_VERSION_ZSTD: c_int = 1 << 26;

pub const CURL_HTTP_VERSION_1_0: c_long = 1;
pub const CURL_HTTP_VERSION_1_1: c_long = 2;
pub const CURL_HTTP_VERSION_2_0: c_long = 3;
pub const CURL_HTTP_VERSION_2TLS: c_long = 4;
pub const CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE: c_long = 5;
pub const CURL_HTTP_VERSION_3: c_long = 30;
pub const CURL_HTTP_VERSION_3ONLY: c_long = 31;

pub const CURL_SSLVERSION_TLSv1: c_long = 1;
pub const CURL_SSLVERSION_TLSv1_0: c_long = 4;
pub const CURL_SSLVERSION_TLSv1_1: c_long = 5;
pub const CURL_SSLVERSION_TLSv1_2: c_long = 6;
pub const CURL_SSLVERSION_TLSv1_3: c_long = 7;

pub const CURLPROXY_HTTP: c_long = 0;
pub const CURLPROXY_HTTPS: c_long = 2;
pub const CURLPROXY_SOCKS4: c_long = 4;
pub const CURLPROXY_SOCKS5: c_long = 5;
pub const CURLPROXY_SOCKS5_HOSTNAME: c_long = 7;

pub const CURL_ZERO_TERMINATED: isize = -1;

pub const CURLH_HEADER: c_uint = 1 << 0;
pub const CURLH_TRAILER: c_uint = 1 << 1;
pub const CURLH_CONNECT: c_uint = 1 << 2;
pub const CURLH_1XX: c_uint = 1 << 3;
pub const CURLH_PSEUDO: c_uint = 1 << 4;

pub const CURLWS_TEXT: c_uint = 1 << 0;
pub const CURLWS_BINARY: c_uint = 1 << 1;
pub const CURLWS_CONT: c_uint = 1 << 2;
pub const CURLWS_CLOSE: c_uint = 1 << 3;
pub const CURLWS_PING: c_uint = 1 << 4;
pub const CURLWS_OFFSET: c_uint = 1 << 5;
pub const CURLWS_PONG: c_uint = 1 << 6;

pub const CURL_BLOB_COPY: c_uint = 1;
pub const CURL_BLOB_NOCOPY: c_uint = 0;

#[cfg(windows)]
pub const CURL_SOCKET_BAD: curl_socket_t = !0;
#[cfg(not(windows))]
pub const CURL_SOCKET_BAD: curl_socket_t = -1;

#[repr(C)]
pub struct curl_slist {
    pub data: *mut c_char,
    pub next: *mut curl_slist,
}

#[repr(C)]
pub struct curl_waitfd {
    pub fd: curl_socket_t,
    pub events: c_short,
    pub revents: c_short,
}

#[cfg(windows)]
pub type c_short = i16;
#[cfg(not(windows))]
pub type c_short = i16;

#[repr(C)]
pub struct CURLMsg {
    pub msg: CURLMSG,
    pub easy_handle: *mut CURL,
    pub data: CURLMsgData,
}

#[repr(C)]
pub union CURLMsgData {
    pub whatever: *mut c_void,
    pub result: CURLcode,
}

impl CURLMsg {
    pub(super) unsafe fn result_code(&self) -> CURLcode {
        self.data.result
    }
}

#[repr(C)]
pub struct curl_blob {
    pub data: *mut c_void,
    pub len: size_t,
    pub flags: c_uint,
}

// Layout mirrors curl's `curl_version_info_data` and MUST match the exact libcurl
// version linked in — curl appends fields across releases and the curlw_verinfo_*
// accessors read fields by offset. Re-check this (and the hand-mirrored CURLMsg /
// curl_header / curl_ws_frame structs) whenever the bundled curl is bumped.
#[repr(C)]
pub struct curl_version_info_data {
    pub age: CURLversion,
    pub version: *const c_char,
    pub version_num: c_uint,
    pub host: *const c_char,
    pub features: c_int,
    pub ssl_version: *const c_char,
    pub ssl_version_num: c_long,
    pub libz_version: *const c_char,
    pub protocols: *const *const c_char,
    pub ares: *const c_char,
    pub ares_num: c_int,
    pub libidn: *const c_char,
    pub iconv_ver_num: c_int,
    pub libssh_version: *const c_char,
    pub brotli_ver_num: c_uint,
    pub brotli_version: *const c_char,
    pub nghttp2_ver_num: c_uint,
    pub nghttp2_version: *const c_char,
    pub quic_version: *const c_char,
    pub cainfo: *const c_char,
    pub capath: *const c_char,
    pub zstd_ver_num: c_uint,
    pub zstd_version: *const c_char,
    pub hyper_version: *const c_char,
    pub gsasl_version: *const c_char,
    pub feature_names: *const *const c_char,
    pub rtmp_version: *const c_char,
}

#[repr(C)]
pub struct curl_header {
    pub name: *mut c_char,
    pub value: *mut c_char,
    pub amount: size_t,
    pub index: size_t,
    pub origin: c_uint,
    pub anchor: *mut c_void,
}

#[repr(C)]
pub struct curl_ws_frame {
    pub age: c_int,
    pub flags: c_int,
    pub offset: curl_off_t,
    pub bytesleft: curl_off_t,
    pub len: size_t,
}

#[cfg(windows)]
#[repr(C)]
pub struct sockaddr {
    pub sa_family: u16,
    pub sa_data: [c_char; 14],
}

#[cfg(not(windows))]
pub use libc::sockaddr;

#[repr(C)]
pub struct curl_sockaddr {
    pub family: c_int,
    pub socktype: c_int,
    pub protocol: c_int,
    pub addrlen: c_uint,
    pub addr: sockaddr,
}

pub type curl_write_callback = unsafe extern "C" fn(*mut c_char, size_t, size_t, *mut c_void) -> size_t;
pub type curl_read_callback = unsafe extern "C" fn(*mut c_char, size_t, size_t, *mut c_void) -> size_t;
pub type curl_opensocket_callback = unsafe extern "C" fn(*mut c_void, curlsocktype, *mut curl_sockaddr) -> curl_socket_t;
pub type curl_closesocket_callback = unsafe extern "C" fn(*mut c_void, curl_socket_t) -> c_int;
pub type curl_socket_callback = unsafe extern "C" fn(*mut CURL, curl_socket_t, c_int, *mut c_void, *mut c_void) -> c_int;
pub type curl_multi_timer_callback = unsafe extern "C" fn(*mut CURLM, c_long, *mut c_void) -> c_int;
pub type curl_lock_function = unsafe extern "C" fn(*mut CURL, curl_lock_data, curl_lock_access, *mut c_void);
pub type curl_unlock_function = unsafe extern "C" fn(*mut CURL, curl_lock_data, *mut c_void);
pub type curlsocktype = c_int;

#[repr(C)]
pub struct timeval {
    pub(super) tv_sec: c_long,
    pub(super) tv_usec: c_long,
}

#[cfg(windows)]
#[repr(C)]
pub struct fd_set {
    pub fd_count: u_int,
    pub fd_array: [curl_socket_t; 64],
}

#[cfg(not(windows))]
pub use libc::fd_set;

pub type u_int = c_uint;

pub const WSAEINTR: c_int = 10004;
pub const WSAETIMEDOUT: c_int = 10060;
pub const SD_BOTH: c_int = 2;

#[cfg(windows)]
extern "system" {
    pub fn WSAGetLastError() -> c_int;
    pub fn WSASetLastError(iErr: c_int);
    pub fn select(
        nfds: c_int,
        readfds: *mut fd_set,
        writefds: *mut fd_set,
        exceptfds: *mut fd_set,
        timeout: *mut timeval,
    ) -> c_int;
    pub fn closesocket(s: curl_socket_t) -> c_int;
    pub fn shutdown(s: curl_socket_t, how: c_int) -> c_int;
    pub fn socket(af: c_int, socktype: c_int, protocol: c_int) -> curl_socket_t;
}

#[cfg(not(windows))]
extern "C" {
    pub fn select(
        nfds: c_int,
        readfds: *mut fd_set,
        writefds: *mut fd_set,
        exceptfds: *mut fd_set,
        timeout: *mut timeval,
    ) -> c_int;
    pub fn socket(af: c_int, socktype: c_int, protocol: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn shutdown(s: c_int, how: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
extern "C" {
    fn __errno_location() -> *mut c_int;
}

#[cfg(target_os = "android")]
extern "C" {
    fn __errno() -> *mut c_int;
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
extern "C" {
    fn __error() -> *mut c_int;
}

#[cfg(windows)]
extern "C" {
    fn _errno() -> *mut c_int;
}

#[inline]
pub(super) unsafe fn errno_ptr() -> *mut c_int {
    #[cfg(target_os = "linux")]
    {
        __errno_location()
    }
    #[cfg(target_os = "android")]
    {
        __errno()
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    {
        __error()
    }
    #[cfg(windows)]
    {
        _errno()
    }
}

extern "C" {
    pub fn curl_version() -> *const c_char;
    pub fn curl_version_info(t: CURLversion) -> *const curl_version_info_data;
    pub fn curl_global_init(flags: c_long) -> CURLcode;
    pub fn curl_global_cleanup();
    pub fn curl_free(p: *mut c_void);

    pub fn curl_easy_init() -> *mut CURL;
    pub fn curl_easy_cleanup(curl: *mut CURL);
    pub fn curl_easy_reset(curl: *mut CURL);
    pub fn curl_easy_duphandle(curl: *mut CURL) -> *mut CURL;
    pub fn curl_easy_perform(curl: *mut CURL) -> CURLcode;
    pub fn curl_easy_pause(handle: *mut CURL, bitmask: c_int) -> CURLcode;
    pub fn curl_easy_upkeep(handle: *mut CURL) -> CURLcode;
    pub fn curl_easy_recv(curl: *mut CURL, buffer: *mut c_void, buflen: size_t, n: *mut size_t) -> CURLcode;
    pub fn curl_easy_send(curl: *mut CURL, buffer: *const c_void, buflen: size_t, n: *mut size_t) -> CURLcode;
    pub fn curl_easy_escape(handle: *mut CURL, string: *const c_char, length: c_int) -> *mut c_char;
    pub fn curl_easy_unescape(handle: *mut CURL, string: *const c_char, inlength: c_int, outlength: *mut c_int) -> *mut c_char;
    pub fn curl_easy_strerror(code: CURLcode) -> *const c_char;

    pub(super) fn curl_easy_setopt(handle: *mut CURL, option: CURLoption, ...) -> CURLcode;
    pub(super) fn curl_easy_getinfo(handle: *mut CURL, info: CURLINFO, ...) -> CURLcode;

    pub fn curl_multi_init() -> *mut CURLM;
    pub fn curl_multi_cleanup(multi_handle: *mut CURLM) -> CURLMcode;
    pub fn curl_multi_add_handle(multi_handle: *mut CURLM, curl_handle: *mut CURL) -> CURLMcode;
    pub fn curl_multi_remove_handle(multi_handle: *mut CURLM, curl_handle: *mut CURL) -> CURLMcode;
    pub fn curl_multi_perform(multi_handle: *mut CURLM, running_handles: *mut c_int) -> CURLMcode;
    pub fn curl_multi_fdset(
        multi_handle: *mut CURLM,
        read_fd_set: *mut fd_set,
        write_fd_set: *mut fd_set,
        exc_fd_set: *mut fd_set,
        max_fd: *mut c_int,
    ) -> CURLMcode;
    pub fn curl_multi_timeout(multi_handle: *mut CURLM, milliseconds: *mut c_long) -> CURLMcode;
    pub fn curl_multi_wait(
        multi_handle: *mut CURLM,
        extra_fds: *mut curl_waitfd,
        extra_nfds: c_uint,
        timeout_ms: c_int,
        ret: *mut c_int,
    ) -> CURLMcode;
    pub fn curl_multi_poll(
        multi_handle: *mut CURLM,
        extra_fds: *mut curl_waitfd,
        extra_nfds: c_uint,
        timeout_ms: c_int,
        numfds: *mut c_int,
    ) -> CURLMcode;
    pub fn curl_multi_wakeup(multi_handle: *mut CURLM) -> CURLMcode;
    pub fn curl_multi_strerror(code: CURLMcode) -> *const c_char;
    pub fn curl_multi_info_read(multi_handle: *mut CURLM, msgs_in_queue: *mut c_int) -> *mut CURLMsg;
    pub fn curl_multi_socket_action(
        multi_handle: *mut CURLM,
        s: curl_socket_t,
        ev_bitmask: c_int,
        running_handles: *mut c_int,
    ) -> CURLMcode;
    pub fn curl_multi_assign(multi_handle: *mut CURLM, sockfd: curl_socket_t, sockp: *mut c_void) -> CURLMcode;
    pub fn curl_multi_get_handles(multi_handle: *mut CURLM) -> *mut *mut CURL;

    pub(super) fn curl_multi_setopt(handle: *mut CURLM, option: CURLMoption, ...) -> CURLMcode;

    pub fn curl_slist_append(list: *mut curl_slist, val: *const c_char) -> *mut curl_slist;
    pub fn curl_slist_free_all(list: *mut curl_slist);

    pub fn curl_share_init() -> *mut CURLSH;
    pub fn curl_share_cleanup(sh: *mut CURLSH) -> CURLSHcode;
    pub fn curl_share_strerror(code: CURLSHcode) -> *const c_char;

    pub(super) fn curl_share_setopt(sh: *mut CURLSH, opt: CURLSHoption, ...) -> CURLSHcode;

    pub fn curl_mime_init(easy: *mut CURL) -> *mut curl_mime;
    pub fn curl_mime_free(mime: *mut curl_mime);
    pub fn curl_mime_addpart(mime: *mut curl_mime) -> *mut curl_mimepart;
    pub fn curl_mime_name(part: *mut curl_mimepart, name: *const c_char) -> CURLcode;
    pub fn curl_mime_data(part: *mut curl_mimepart, data: *const c_char, datasize: size_t) -> CURLcode;
    pub fn curl_mime_filedata(part: *mut curl_mimepart, filename: *const c_char) -> CURLcode;
    pub fn curl_mime_filename(part: *mut curl_mimepart, filename: *const c_char) -> CURLcode;
    pub fn curl_mime_type(part: *mut curl_mimepart, mimetype: *const c_char) -> CURLcode;
    pub fn curl_mime_encoder(part: *mut curl_mimepart, encoding: *const c_char) -> CURLcode;
    pub fn curl_mime_headers(part: *mut curl_mimepart, headers: *mut curl_slist, take_ownership: c_int) -> CURLcode;
    pub fn curl_mime_subparts(part: *mut curl_mimepart, subparts: *mut curl_mime) -> CURLcode;

    pub fn curl_url() -> *mut CURLU;
    pub fn curl_url_cleanup(handle: *mut CURLU);
    pub fn curl_url_dup(in_: *const CURLU) -> *mut CURLU;
    pub fn curl_url_get(handle: *const CURLU, what: CURLUPart, part: *mut *mut c_char, flags: c_uint) -> CURLUcode;
    pub fn curl_url_set(handle: *mut CURLU, what: CURLUPart, part: *const c_char, flags: c_uint) -> CURLUcode;
    pub fn curl_url_strerror(error: CURLUcode) -> *const c_char;

    pub fn curl_easy_header(
        handle: *mut CURL,
        name: *const c_char,
        index: size_t,
        origin: c_uint,
        request: c_int,
        hout: *mut *mut curl_header,
    ) -> CURLHcode;
    pub fn curl_easy_nextheader(
        handle: *mut CURL,
        origin: c_uint,
        request: c_int,
        prev: *mut curl_header,
    ) -> *mut curl_header;

    pub fn curl_ws_recv(
        handle: *mut CURL,
        buffer: *mut c_void,
        buflen: size_t,
        recv: *mut size_t,
        meta: *mut *const curl_ws_frame,
    ) -> CURLcode;
    pub fn curl_ws_send(
        handle: *mut CURL,
        buffer: *const c_void,
        buflen: size_t,
        sent: *mut size_t,
        fragsize: curl_off_t,
        flags: c_uint,
    ) -> CURLcode;
    pub fn curl_ws_meta(handle: *mut CURL) -> *const curl_ws_frame;
}

// libcurl's setopt/getinfo are C variadic functions. Call them directly rather
// than transmuting to a fixed-arity fn pointer: on the Apple arm64 ABI variadic
// arguments are passed on the stack, so a fixed-arity call would place them in
// registers and libcurl would read garbage. A direct variadic call lets the
// compiler honor each target's varargs convention.
#[inline(always)]
pub(super) unsafe fn curl_easy_setopt_long(handle: *mut CURL, option: CURLoption, value: c_long) -> CURLcode {
    curl_easy_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_setopt_offt(handle: *mut CURL, option: CURLoption, value: curl_off_t) -> CURLcode {
    curl_easy_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_setopt_ptr(handle: *mut CURL, option: CURLoption, value: *mut c_void) -> CURLcode {
    curl_easy_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_getinfo_long(handle: *mut CURL, info: CURLINFO, value: *mut c_long) -> CURLcode {
    curl_easy_getinfo(handle, info, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_getinfo_double(handle: *mut CURL, info: CURLINFO, value: *mut c_double) -> CURLcode {
    curl_easy_getinfo(handle, info, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_getinfo_ptr(handle: *mut CURL, info: CURLINFO, value: *mut c_void) -> CURLcode {
    curl_easy_getinfo(handle, info, value)
}

#[inline(always)]
pub(super) unsafe fn curl_easy_getinfo_offt(handle: *mut CURL, info: CURLINFO, value: *mut curl_off_t) -> CURLcode {
    curl_easy_getinfo(handle, info, value)
}

#[inline(always)]
pub(super) unsafe fn curl_multi_setopt_long(handle: *mut CURLM, option: CURLMoption, value: c_long) -> CURLMcode {
    curl_multi_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_multi_setopt_offt(handle: *mut CURLM, option: CURLMoption, value: curl_off_t) -> CURLMcode {
    curl_multi_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_multi_setopt_ptr(handle: *mut CURLM, option: CURLMoption, value: *mut c_void) -> CURLMcode {
    curl_multi_setopt(handle, option, value)
}

#[inline(always)]
pub(super) unsafe fn curl_share_setopt_int(sh: *mut CURLSH, opt: CURLSHoption, value: c_int) -> CURLSHcode {
    curl_share_setopt(sh, opt, value)
}

#[inline(always)]
pub(super) unsafe fn curl_share_setopt_ptr(sh: *mut CURLSH, opt: CURLSHoption, value: *mut c_void) -> CURLSHcode {
    curl_share_setopt(sh, opt, value)
}
