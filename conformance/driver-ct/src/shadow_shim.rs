//! In-binary overrides of the libc syscall wrappers whose Shadow-simulator
//! behavior breaks the `webrtc` 0.21 driver's quinn-udp UDP layer. Symbols
//! defined in the executable win dynamic-symbol resolution over every
//! preloaded library — including Shadow's `preload-libc` — so overriding here
//! works identically inside and outside Shadow.
//!
//! Every override **forwards the call as received** first, through
//! `dlsym(RTLD_NEXT)`. The stubs engage only when *both* hold:
//!
//! - the [`SHADOW_SYSCALL_SHIM_ENV`] environment variable is set — the Shadow
//!   executor sets it on the peer processes it places inside the simulation,
//!   so every other environment (loopback, netns) gets pure pass-through, and
//! - the forwarded call fails with the exact error Shadow's unimplemented
//!   path produces:
//!
//!   - `setsockopt(IPPROTO_IP, {IP_PKTINFO, IP_MTU_DISCOVER, IP_RECVTOS})` —
//!     Shadow rejects these with `ENOPROTOOPT`, and quinn-udp treats an
//!     `IP_PKTINFO` failure as fatal to socket construction. The stub reports
//!     success: the options only enable optional receive metadata that Shadow
//!     never delivers anyway, a shape quinn-udp already handles. A targeted
//!     option failing with any *other* error aborts the peer rather than
//!     stubbing over an unknown environment.
//!   - `recvmmsg` — Shadow does not implement it (`ENOSYS`), and quinn-udp's
//!     Linux receive path calls it with no fallback. After `ENOSYS` is
//!     observed once, the stub emulates a one-message batch via `recvmsg`;
//!     other errors (`EAGAIN`, ...) are forwarded untouched.
//!
//! Scope: IPv4 options only, and exactly the syscalls quinn-udp 0.6 uses. If
//! a future `webrtc`/quinn-udp bump grows the syscall surface, the symptom is
//! the peer aborting (targeted `setsockopt`) or Shadow's "unsupported syscall"
//! warning plus a hang, and this module is where to extend the bridge. If
//! Shadow instead grows support for these syscalls (or an upstream
//! quinn-udp/webrtc release tolerates their absence), this shim can be retired.

use std::ffi::{c_int, c_uint, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Arms the stubs; set by the Shadow executor on simulated peers.
pub const SHADOW_SYSCALL_SHIM_ENV: &str = "CONFORMANCE_SHADOW_SYSCALL_SHIM";

/// Whether the stubs may engage: true when [`SHADOW_SYSCALL_SHIM_ENV`] is set
/// (checked once). When false, every override is pure pass-through.
fn shim_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os(SHADOW_SYSCALL_SHIM_ENV).is_some())
}

/// Resolve the next definition of `name` after this executable (Shadow's
/// `preload-libc` when present, else libc), caching the address in `cell`.
fn next_fn(cell: &OnceLock<usize>, name: &'static CStr) -> usize {
    *cell.get_or_init(|| {
        let addr = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
        assert!(
            !addr.is_null(),
            "shadow-syscall-shim: dlsym(RTLD_NEXT, {name:?}) found no next definition"
        );
        addr as usize
    })
}

/// Whether this `setsockopt` target is one Shadow rejects but quinn-udp needs
/// to "succeed": the optional-receive-metadata options on IPv4 UDP sockets.
fn stubbed_ip_option(level: c_int, optname: c_int) -> bool {
    level == libc::IPPROTO_IP
        && matches!(
            optname,
            libc::IP_PKTINFO | libc::IP_MTU_DISCOVER | libc::IP_RECVTOS
        )
}

type SetsockoptFn =
    unsafe extern "C" fn(c_int, c_int, c_int, *const c_void, libc::socklen_t) -> c_int;

/// Override of libc `setsockopt`: forward as received; when the shim is
/// enabled and a targeted receive-metadata option fails, report success on
/// Shadow's `ENOPROTOOPT` and abort on anything else (the abort also flags a
/// future Shadow that starts handling these options differently).
///
/// # Safety
///
/// Same contract as libc `setsockopt`; called by foreign code through the
/// dynamic linker.
#[no_mangle]
pub unsafe extern "C" fn setsockopt(
    fd: c_int,
    level: c_int,
    optname: c_int,
    optval: *const c_void,
    optlen: libc::socklen_t,
) -> c_int {
    static NEXT: OnceLock<usize> = OnceLock::new();
    let next: SetsockoptFn = unsafe { std::mem::transmute(next_fn(&NEXT, c"setsockopt")) };
    // Resolved before forwarding so nothing runs between the forwarded call
    // and the caller-visible errno.
    let enabled = shim_enabled();

    let ret = unsafe { next(fd, level, optname, optval, optlen) };
    if ret == 0 || !enabled || !stubbed_ip_option(level, optname) {
        return ret;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOPROTOOPT) {
        // Shadow's documented rejection of an option it does not implement:
        // report success. The option only enables receive metadata that is
        // simply never delivered, which the callers handle.
        return 0;
    }
    eprintln!(
        "shadow-syscall-shim: setsockopt(fd={fd}, level={level}, opt={optname}) failed with \
         unexpected error {err} (expected success on a real kernel or ENOPROTOOPT under \
         Shadow); aborting"
    );
    std::process::abort();
}

type RecvmmsgFn =
    unsafe extern "C" fn(c_int, *mut libc::mmsghdr, c_uint, c_int, *mut libc::timespec) -> c_int;
type RecvmsgFn = unsafe extern "C" fn(c_int, *mut libc::msghdr, c_int) -> libc::ssize_t;

/// Override of libc `recvmmsg`: forward as received; when the shim is enabled
/// and a forwarded call fails with Shadow's `ENOSYS`, emulate a one-message
/// batch via `recvmsg` from then on. Any other outcome — success or a genuine
/// runtime error such as `EAGAIN` — is forwarded untouched.
///
/// The emulation ignores `timeout`: it receives at most one message, and
/// `recvmmsg`'s timeout only bounds waiting *between* messages of a batch
/// (quinn-udp always passes null).
///
/// # Safety
///
/// Same contract as libc `recvmmsg`; called by foreign code through the
/// dynamic linker.
#[no_mangle]
pub unsafe extern "C" fn recvmmsg(
    fd: c_int,
    msgvec: *mut libc::mmsghdr,
    vlen: c_uint,
    flags: c_int,
    timeout: *mut libc::timespec,
) -> c_int {
    static NEXT: OnceLock<usize> = OnceLock::new();
    static ENOSYS_SEEN: AtomicBool = AtomicBool::new(false);

    if !ENOSYS_SEEN.load(Ordering::Relaxed) {
        let next: RecvmmsgFn = unsafe { std::mem::transmute(next_fn(&NEXT, c"recvmmsg")) };
        // Resolved before forwarding so nothing runs between the forwarded
        // call and the caller-visible errno.
        let enabled = shim_enabled();
        let ret = unsafe { next(fd, msgvec, vlen, flags, timeout) };
        if ret >= 0
            || !enabled
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS)
        {
            return ret;
        }
        // Shadow's unimplemented-syscall error, observed: emulate from now on.
        ENOSYS_SEEN.store(true, Ordering::Relaxed);
    }

    if msgvec.is_null() || vlen == 0 {
        unsafe { *libc::__errno_location() = libc::EINVAL };
        return -1;
    }
    static NEXT_RECVMSG: OnceLock<usize> = OnceLock::new();
    let next_recvmsg: RecvmsgFn =
        unsafe { std::mem::transmute(next_fn(&NEXT_RECVMSG, c"recvmsg")) };
    // `MSG_WAITFORONE` is recvmmsg-only; the emulated batch is one message, so
    // its "return after the first message" semantics hold trivially.
    let flags = flags & !libc::MSG_WAITFORONE;
    let msg = unsafe { &mut *msgvec };
    let n = unsafe { next_recvmsg(fd, &mut msg.msg_hdr, flags) };
    if n < 0 {
        return -1;
    }
    msg.msg_len = n as c_uint;
    1
}
