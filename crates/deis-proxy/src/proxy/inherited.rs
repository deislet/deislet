//! Making pingora's table of inherited descriptors agree with this
//! configuration.
//!
//! # What arrives, and what asks for it
//!
//! A zero-downtime upgrade hands the replacement one thing and one thing
//! only: `pingora::server::Fds`, a map from the bind address *as a string* to
//! an open descriptor. Nothing else crosses the upgrade socket - not the
//! certificates, not the routing table, not the configuration. Everything
//! else the replacement has, it read for itself.
//!
//! The whole map crosses, every time. What the replacement then asks for is
//! decided one address at a time, inside pingora: building a listener for
//! address `A` looks `A` up in the map, adopts the descriptor if it is there
//! and binds a new socket if it is not. There is no step anywhere that asks
//! what is *left* in the map afterwards.
//!
//! # The hole that leaves
//!
//! Drop an address from `listen_ports` and reload. The old process hands over
//! both descriptors, the new process asks for one, and the other stays open
//! in the new process for ever - bound, listening, and attached to no
//! acceptor. Every consequence of that is worse than the port simply being
//! gone:
//!
//! * a client connecting there completes the TCP handshake and then waits for
//!   a response nobody will ever write, so decommissioning a port hangs
//!   clients instead of refusing them;
//! * a TCP-level health check passes, so a load balancer in front keeps
//!   sending traffic into the hang;
//! * whatever was meant to take the port over gets `EADDRINUSE` from a
//!   process whose own log says it is not listening there;
//! * it survives the next upgrade, and the one after that, because the map is
//!   handed on whole.
//!
//! [`reconcile`] is the missing step. It runs once, in the replacement, after
//! the descriptors have arrived and before any listener is built.
//!
//! # Why an entry cannot simply be dropped
//!
//! `Fds` has `new`, `add`, `get` and `serialize`, and no way to remove a key.
//! So closing an unwanted descriptor would leave the map naming a number that
//! is no longer open, and the *next* upgrade sends every number in the map
//! through `sendmsg` with `SCM_RIGHTS`: one closed descriptor there is
//! `EBADF` for the whole message, the replacement receives nothing, and it
//! exits after the running process has already been told it may leave. That
//! is a total outage, produced by the fix rather than by the bug.
//!
//! So the descriptor is not closed - it is *replaced in place*, with `dup3`
//! of `/dev/null` onto the same number. The listening socket is closed by
//! that `dup3`, which is what releases the port; the number stays open, so
//! the next transfer still succeeds; and `/dev/null` is self-describing in
//! `/proc/<pid>/fd` for whoever is reading it during an incident. `dup3`
//! rather than `dup2` because the flags matter here; see "The other copy"
//! below.
//!
//! # Which makes the table untrustworthy, so it is checked
//!
//! A map entry is therefore a *claim*: it says "this number is the listening
//! socket for this address". After one reconciliation some entries are
//! claims about `/dev/null`, and a configuration that re-adds a dropped
//! address would otherwise hand `/dev/null` to `TcpSocket::listen` and fail
//! the upgrade at the one moment it cannot be failed.
//!
//! Nothing here believes the claim. Every entry is checked against the kernel
//! ([`classify`]) before anything is done with it, and there are exactly four
//! answers:
//!
//! | in the configuration | is what it claims | what happens |
//! |---|---|---|
//! | yes | yes | left alone; pingora adopts it |
//! | yes | no  | a fresh listener is bound and put in its place |
//! | no  | yes | released: `/dev/null` over it, port freed |
//! | no  | no  | left alone - this process did not put it there |
//!
//! The check is also what makes the release safe to do at all. `dup3` onto a
//! number that turned out to be the error log would be a far worse bug than
//! the one being fixed, so a number is only ever written over once the kernel
//! has confirmed it is a listening TCP socket bound to the address the map
//! claims for it.
//!
//! # The other copy: descriptors that cross `execve`
//!
//! Releasing the map's copy of a listener is not enough on its own, because
//! it is not always the only copy. Pingora receives the transferred
//! descriptors with `recvmsg(..., MsgFlags::empty())`, without
//! `MSG_CMSG_CLOEXEC`, so every descriptor that arrives over the upgrade
//! socket has `FD_CLOEXEC` *clear*. [`crate::restart::graceful_upgrade`] then
//! starts the next replacement with `std::process::Command`, and everything
//! without `FD_CLOEXEC` is inherited straight across the `execve` as well.
//!
//! So from the second reload onwards each generation holds two copies of
//! every inherited listener: the one handed to it over the upgrade socket,
//! which is in the map, and one that simply came along through `exec`, which
//! is not. Two things follow, and both were measured on this repository's own
//! binary:
//!
//! * the descriptors accumulate, one more per table entry per reload, for as
//!   long as the instance is ever reloaded; and
//! * releasing the map's copy of a dropped address does **not** free the
//!   port, because the copy that came through `exec` is still holding it. The
//!   defect this module exists for survives at every reload after the first.
//!
//! Both are closed here, and the discriminator is exact. A socket this
//! process bound for itself always has `FD_CLOEXEC` - Rust and tokio set it
//! on every socket they create - so a listening TCP socket that is open, is
//! *not* named by the map, and has `FD_CLOEXEC` clear cannot be anything but
//! a descriptor that survived an `execve`. Nothing else in the process can
//! look like that. So:
//!
//! * every descriptor the map names is given `FD_CLOEXEC`, which stops this
//!   generation leaking into the next one without changing anything about how
//!   it is used - `SCM_RIGHTS` is not `exec`, so the transfer is unaffected;
//! * an `exec`-leaked listener at an address this configuration does not want
//!   is closed, which is what actually frees the port; and
//! * an `exec`-leaked listener at an address it *does* want is a harmless
//!   duplicate of one pingora is about to adopt, so it is only marked, never
//!   closed.

use std::fs::OpenOptions;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use async_trait::async_trait;
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::sys::socket::{SockType, SockaddrStorage, getsockname, getsockopt, sockopt};
use pingora::server::{ListenFds, ShutdownWatch};
use pingora::services::Service;
use tokio::net::TcpSocket;
use tracing::{debug, error, info, warn};

/// Backlog for a listener bound by [`rebind`].
///
/// Not a copy of pingora's own `LISTENER_BACKLOG`, and it does not have to
/// agree with it: pingora calls `listen()` again on whatever descriptor it
/// adopts, so the backlog this socket is created with is overwritten by
/// pingora's the moment it takes it over. It only has to be a legal value.
const REBIND_BACKLOG: u32 = 1024;

/// What one entry of the inherited table turned out to be, according to the
/// kernel rather than according to the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inherited {
    /// A listening TCP socket whose local address is the one the table claims
    /// for it.
    ListeningAt(SocketAddr),
    /// Anything else. The string says what, and is written for a log line an
    /// operator reads during an upgrade.
    NotWhatItClaims(String),
}

/// What [`reconcile`] did about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// In the configuration and genuinely the listener it claims to be.
    /// Untouched; pingora adopts it a moment later.
    Kept(SocketAddr),
    /// Not in the configuration any more. The listening socket was closed and
    /// the port released; the descriptor number now holds `/dev/null` so the
    /// next upgrade can still carry the map.
    Released(SocketAddr),
    /// In the configuration, but the descriptor is not the listener the table
    /// claims. A fresh listener was bound and put in its place.
    Rebound,
    /// In the configuration, the descriptor is wrong, and a fresh listener
    /// could not be bound either. Left as it was: pingora will fail on it and
    /// [`super::ExitOnPanic`] will make that failure visible, which is the
    /// most this can do.
    Unusable(String),
    /// Not in the configuration, and not the listener it claims to be either.
    /// Left alone: this process has no evidence about what that number is and
    /// writing over it would be the worse bug.
    LeftAlone(String),
}

/// Ask the kernel what `fd` actually is, and whether it is what `bind` says.
///
/// Every question here is one the table cannot answer: it holds a number and
/// a string, and both were chosen by a different process.
///
/// `getsockname` alone is not enough. A connection *accepted* on
/// `127.0.0.1:8080` has exactly that local address, so an address match on
/// its own would let a live request's socket be mistaken for the listener it
/// arrived on. `SO_ACCEPTCONN` is the question that separates them.
pub fn classify(fd: RawFd, bind: &str) -> Inherited {
    let local = match listening_at(fd) {
        Ok(local) => local,
        Err(why) => return Inherited::NotWhatItClaims(why),
    };
    match resolve(bind) {
        Ok(claimed) if claimed.contains(&local) => Inherited::ListeningAt(local),
        Ok(_) => Inherited::NotWhatItClaims(format!(
            "a listening socket at {local}, which is not {bind}"
        )),
        Err(e) => Inherited::NotWhatItClaims(format!("{bind} does not resolve: {e}")),
    }
}

/// The address `fd` is listening on, or why it is not a listening TCP socket.
///
/// Three questions, none of which the table can answer, and all three are
/// needed. `getsockname` alone would accept a datagram socket and, worse, a
/// connection *accepted* on `127.0.0.1:8080` - which has exactly that local
/// address, so an address match on its own would let a live request's socket
/// be mistaken for the listener it arrived on. `SO_ACCEPTCONN` is what
/// separates those two.
pub fn listening_at(fd: RawFd) -> Result<SocketAddr, String> {
    // SAFETY: borrowed for the duration of the calls below and never closed
    // here; the caller owns the descriptor.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

    let local: SockaddrStorage =
        // ENOTSOCK for a plain file, EBADF for a number that is not open at
        // all. Both mean the same thing here: not a listener, do not touch.
        getsockname(fd).map_err(|e| format!("getsockname failed: {e}"))?;
    let local = match (local.as_sockaddr_in(), local.as_sockaddr_in6()) {
        (Some(v4), _) => SocketAddr::from(*v4),
        (_, Some(v6)) => SocketAddr::from(*v6),
        _ => {
            return Err("a socket that is not an IP socket (a Unix socket, say)".to_string());
        }
    };
    match getsockopt(&borrowed, sockopt::SockType) {
        Ok(SockType::Stream) => {}
        Ok(other) => return Err(format!("a {other:?} socket, not a stream socket")),
        Err(e) => return Err(format!("SO_TYPE failed: {e}")),
    }
    match getsockopt(&borrowed, sockopt::AcceptConn) {
        Ok(true) => Ok(local),
        Ok(false) => Err(format!("a TCP socket at {local} that is not listening")),
        Err(e) => Err(format!("SO_ACCEPTCONN failed: {e}")),
    }
}

/// Whether `fd` would survive an `execve`.
///
/// The whole of the discriminator in "The other copy" above: a socket this
/// process created has `FD_CLOEXEC`, because Rust and tokio set it on
/// everything they open; one that arrived over pingora's upgrade socket does
/// not, because `recvmsg` was not given `MSG_CMSG_CLOEXEC`.
fn crosses_exec(fd: RawFd) -> bool {
    // SAFETY: reads the descriptor flags and nothing else.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    match fcntl(borrowed, FcntlArg::F_GETFD) {
        Ok(flags) => !FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC),
        // Not open. Nothing can be inherited from it either.
        Err(_) => false,
    }
}

/// Stop `fd` being inherited by anything this process starts.
///
/// Safe on a listener in use: `FD_CLOEXEC` is consulted by `execve` and by
/// nothing else, so the descriptor keeps working here, and `SCM_RIGHTS` -
/// which is how it is *meant* to reach the next generation - is unaffected.
fn stop_crossing_exec(fd: RawFd) -> Result<(), String> {
    // SAFETY: sets a descriptor flag; nothing is closed.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    fcntl(borrowed, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(|_| ())
        .map_err(|e| format!("setting FD_CLOEXEC: {e}"))
}

/// The addresses a bind string names.
///
/// Almost always a parse: the strings in this table are built by
/// `SystemConfig::listen_addresses` out of a literal address and a port. The
/// fallback is the same `to_socket_addrs` pingora itself calls when it binds
/// one, so a host name spelled in `listen_addr` is understood here exactly as
/// it is there rather than being rejected by a stricter second reader.
fn resolve(bind: &str) -> std::io::Result<Vec<SocketAddr>> {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    bind.to_socket_addrs().map(|addrs| addrs.collect())
}

/// Close the listening socket at `fd` and leave `/dev/null` at that number.
///
/// The two halves are one `dup3`, which is what makes it safe to do while
/// another thread could be looking at the table: the number never stops being
/// open, so nothing can find it closed and nothing can have it handed back by
/// a later `open`.
///
/// `O_CLOEXEC` rather than a plain `dup2`, because `dup2` clears the flag on
/// the descriptor it creates. Without it the placeholder would be inherited
/// by the next replacement across `execve` - a leak in the machinery whose
/// whole purpose is to stop one.
fn release(fd: RawFd) -> Result<(), String> {
    let devnull = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| format!("opening /dev/null to put over the descriptor: {e}"))?;
    // Wrapped only so `dup3` can take the `&mut OwnedFd` it wants. Ownership
    // stays with pingora's table either way: `into_raw_fd` below is what
    // stops this wrapper closing the number when it goes out of scope, which
    // would be exactly the dangling entry this whole approach exists to
    // avoid.
    let mut slot = unsafe { OwnedFd::from_raw_fd(fd) };
    let result = nix::unistd::dup3(devnull.as_fd(), &mut slot, OFlag::O_CLOEXEC)
        .map_err(|e| format!("dup3 of /dev/null over the descriptor: {e}"));
    let _ = slot.into_raw_fd();
    result
}

/// What was found holding a port outside pingora's table, and what was done
/// about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaked {
    /// At an address this configuration does not want. Closed, port released.
    Closed,
    /// At an address this configuration does want, so it is a second copy of
    /// a listener pingora is about to adopt from the table. Left open and
    /// only stopped from crossing the next `execve`.
    Marked,
    /// Neither could be done. Named so the descriptor is at least visible.
    Stuck(String),
}

/// Descriptors this process holds right now.
///
/// `/proc/self/fd`, which is the only way to ask. A failure to read it is not
/// fatal - it costs the sweep below and nothing else - so it is reported
/// rather than returned as an error.
fn open_descriptors() -> Result<Vec<RawFd>, String> {
    let entries =
        std::fs::read_dir("/proc/self/fd").map_err(|e| format!("reading /proc/self/fd: {e}"))?;
    let mut fds = Vec::new();
    for entry in entries.flatten() {
        if let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<RawFd>().ok())
        {
            fds.push(fd);
        }
    }
    fds.sort_unstable();
    Ok(fds)
}

/// Deal with listeners that reached this process through `execve` rather than
/// through the upgrade socket.
///
/// `candidates` is every descriptor this process holds; `in_table` is the set
/// pingora's map names. A candidate is only touched when all three of these
/// hold, and together they can be true of nothing else (see "The other copy"
/// in the module documentation):
///
/// 1. it is a listening TCP socket;
/// 2. the map does not name it, so pingora will neither adopt it nor hand it
///    on - at this point in startup pingora has not built a listener yet, so
///    there is no third owner it could belong to; and
/// 3. `FD_CLOEXEC` is clear, which no socket this process opened for itself
///    ever has.
fn sweep_exec_leaks(
    candidates: &[RawFd],
    in_table: &[RawFd],
    wanted: &[String],
) -> Vec<(RawFd, SocketAddr, Leaked)> {
    let mut found = Vec::new();
    for &fd in candidates {
        if in_table.contains(&fd) || !crosses_exec(fd) {
            continue;
        }
        let Ok(addr) = listening_at(fd) else {
            continue;
        };
        let claimed = wanted
            .iter()
            .any(|address| resolve(address).is_ok_and(|addrs| addrs.contains(&addr)));
        let outcome = if claimed {
            match stop_crossing_exec(fd) {
                Ok(()) => Leaked::Marked,
                Err(e) => Leaked::Stuck(e),
            }
        } else {
            // Closing it is the whole point: this descriptor, and not the one
            // in the table, is what has been holding the port of an address
            // the configuration dropped.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            Leaked::Closed
        };
        found.push((fd, addr, outcome));
    }
    found
}

/// Bind a fresh listener for `bind` and hand back its descriptor.
///
/// `SO_REUSEADDR` is set for the same reason pingora sets it on every
/// listener it binds: the address may still be in `TIME_WAIT` from
/// connections the process being replaced served.
async fn rebind(bind: &str) -> Result<RawFd, String> {
    let addr = *resolve(bind)
        .map_err(|e| format!("{bind} does not resolve: {e}"))?
        .first()
        .ok_or_else(|| format!("{bind} resolves to no address"))?;
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .map_err(|e| format!("creating a socket for {addr}: {e}"))?;
    socket
        .set_reuseaddr(true)
        .map_err(|e| format!("SO_REUSEADDR on {addr}: {e}"))?;
    socket
        .bind(addr)
        .map_err(|e| format!("binding {addr}: {e}"))?;
    let listener = socket
        .listen(REBIND_BACKLOG)
        .map_err(|e| format!("listening on {addr}: {e}"))?;
    Ok(listener
        .into_std()
        .map_err(|e| format!("taking {addr} out of the runtime: {e}"))?
        .into_raw_fd())
}

/// Everything one reconciliation found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// One per entry of pingora's table, in the table's own order.
    pub entries: Vec<(String, Reconciled)>,
    /// Listeners that reached this process through `execve` rather than
    /// through the upgrade socket. Empty once every generation of a fleet is
    /// running this code.
    pub leaks: Vec<(RawFd, SocketAddr, Leaked)>,
    /// Why the `execve` sweep did not run, when it did not.
    pub sweep_skipped: Option<String>,
}

/// Make the inherited table describe what this process is going to listen on,
/// and take away every other hold it has on a port it no longer wants.
///
/// `wanted` is `SystemConfig::listen_addresses` - the same function that
/// spells the addresses handed to `add_tcp`/`add_tls_with_settings` and the
/// same one that tells the replacement which addresses it will be handed, so
/// there is no second spelling of the rule to drift.
///
/// Safe to call with an empty table: a start that is not a replacement has
/// one, and then there is nothing in `/proc/self/fd` that can have crossed an
/// `execve` either.
pub async fn reconcile(table: &ListenFds, wanted: &[String]) -> Reconciliation {
    reconcile_with(table, wanted, open_descriptors()).await
}

/// [`reconcile`], with the list of descriptors this process holds passed in.
///
/// Split out for the tests, which must not let the sweep loose on a whole
/// test binary's descriptors: one test's simulated `exec` leak would be
/// another test's socket.
async fn reconcile_with(
    table: &ListenFds,
    wanted: &[String],
    candidates: Result<Vec<RawFd>, String>,
) -> Reconciliation {
    // Held across the sweep as well as the table walk. Pingora takes this
    // same lock to build each listener, so holding it is what guarantees the
    // sweep's second condition - that the map is the only owner of a
    // listening socket at this moment.
    let mut fds = table.lock().await;
    let (binds, numbers) = fds.serialize();
    let mut entries = Vec::with_capacity(binds.len());
    let mut in_table = Vec::with_capacity(binds.len());

    for (bind, fd) in binds.into_iter().zip(numbers.iter().copied()) {
        // Before anything else, and whatever this entry turns out to be.
        // Everything the map names must reach the next generation over the
        // upgrade socket or not at all; a second copy arriving through
        // `execve` is what made releasing a port not actually release it.
        if let Err(e) = stop_crossing_exec(fd) {
            warn!(bind = %bind, fd, error = %e, "could not stop this descriptor crossing an exec");
        }
        in_table.push(fd);
        let keep = wanted.iter().any(|address| address == &bind);
        let outcome = match (classify(fd, &bind), keep) {
            (Inherited::ListeningAt(addr), true) => Reconciled::Kept(addr),
            (Inherited::ListeningAt(addr), false) => match release(fd) {
                Ok(()) => Reconciled::Released(addr),
                Err(e) => Reconciled::Unusable(e),
            },
            (Inherited::NotWhatItClaims(why), true) => match rebind(&bind).await {
                Ok(fresh) => {
                    fds.add(bind.clone(), fresh);
                    // The number the map used to name is now named by
                    // nothing, so it can go - otherwise every removal and
                    // re-addition of an address would leave one more
                    // `/dev/null` behind for the life of the fleet. Closed
                    // only when this entry was the sole reference to it: two
                    // keys naming one number is not a shape this code
                    // produces, and a descriptor another key still names is
                    // not this entry's to close.
                    //
                    // After the rebind, not before, so the fresh listener
                    // cannot be handed back the very number being retired.
                    if numbers.iter().filter(|n| **n == fd).count() == 1 {
                        drop(unsafe { OwnedFd::from_raw_fd(fd) });
                    }
                    debug!(bind = %bind, was = %why, fd = fresh, "bound a fresh listener");
                    Reconciled::Rebound
                }
                Err(e) => Reconciled::Unusable(format!("{why}; and {e}")),
            },
            (Inherited::NotWhatItClaims(why), false) => Reconciled::LeftAlone(why),
        };
        entries.push((bind, outcome));
    }

    // The descriptors the table names have just been given `FD_CLOEXEC`, so
    // they cannot be mistaken for leaks even by address; the number check
    // below is belt and braces.
    let (leaks, sweep_skipped) = match candidates {
        Ok(candidates) => (sweep_exec_leaks(&candidates, &in_table, wanted), None),
        Err(e) => (Vec::new(), Some(e)),
    };
    Reconciliation {
        entries,
        leaks,
        sweep_skipped,
    }
}

/// Say what [`reconcile`] did, at the level each answer deserves.
///
/// Split from `reconcile` so the decisions can be tested without a subscriber
/// and so every level is chosen in one place. A released port is a `warn`:
/// nothing else in the log says a port this instance used to answer on has
/// stopped answering, and that is a change an operator may be about to be
/// paged about.
fn announce(result: &Reconciliation, wanted: &[String]) {
    let outcomes = &result.entries;
    for (fd, addr, leak) in &result.leaks {
        match leak {
            Leaked::Closed => warn!(
                fd, %addr,
                "a listening socket for this address was still open here after arriving through \
                 exec rather than through the upgrade socket, and this configuration does not \
                 name the address. It has been closed and the port released. Until this build, \
                 that copy is what kept a decommissioned port bound however many times the \
                 instance was reloaded, because the copy in pingora's table was not the only one"
            ),
            Leaked::Marked => info!(
                fd, %addr,
                "a second copy of this listener arrived through exec rather than through the \
                 upgrade socket. The address is still in the configuration, so the copy is \
                 harmless and is kept; it is only stopped from being inherited again, which is \
                 what made these accumulate one per reload"
            ),
            Leaked::Stuck(why) => error!(
                fd, %addr, why,
                "a listener that arrived through exec can neither be closed nor stopped from \
                 crossing the next one; it will go on holding this port and being inherited"
            ),
        }
    }
    if let Some(why) = &result.sweep_skipped {
        warn!(
            why,
            "could not look for listeners that arrived through exec rather than through the \
             upgrade socket. An address removed from the configuration may still be held by one \
             of them, and `ss -ltnp` on this pid is what shows it"
        );
    }
    if outcomes.is_empty() {
        debug!(
            wanted = ?wanted,
            "no listening descriptors were inherited; every address is bound by this process"
        );
        return;
    }
    for (bind, outcome) in outcomes {
        match outcome {
            Reconciled::Kept(addr) => debug!(
                %bind, %addr,
                "taking over the inherited listener for this address"
            ),
            Reconciled::Released(addr) => warn!(
                %bind, %addr,
                "this address was handed over by the process being replaced and is not in this \
                 configuration, so its listener has been closed and the port released. Nothing \
                 answers there once the old process has finished its grace period; a client that \
                 connects gets a refusal rather than the silent hang an orphaned listener would \
                 have given it"
            ),
            Reconciled::Rebound => warn!(
                %bind,
                "the descriptor handed over for this address was not the listener the table \
                 claimed, so a fresh listener has been bound for it. That is the expected shape \
                 when an address is removed from the configuration and later put back: the \
                 removal released the socket and left a placeholder that the table still names"
            ),
            Reconciled::Unusable(why) => error!(
                %bind, why,
                "the descriptor handed over for this address cannot be used and no replacement \
                 could be bound; this listener will fail to start"
            ),
            Reconciled::LeftAlone(why) => warn!(
                %bind, why,
                "an inherited descriptor is not in this configuration and is not the listener the \
                 table claims it is, so it has been left alone rather than written over. It holds \
                 no port of this process's; if it accumulates across upgrades, that is the thing \
                 to look at"
            ),
        }
    }
    let released = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Reconciled::Released(_)))
        .count();
    let kept = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Reconciled::Kept(_)))
        .count();
    info!(
        inherited = outcomes.len(),
        kept,
        released,
        exec_leaks = result.leaks.len(),
        wanted = ?wanted,
        "reconciled the descriptors handed over by the process being replaced with this \
         configuration"
    );
}

/// Runs [`reconcile`] against the inherited table before the wrapped service
/// builds a single listener.
///
/// A wrapper rather than a line in `main`, because the table only exists
/// inside `Service::start_service`: `Server::listen_fds` is private and the
/// handle reaches user code exactly once, as this argument. It has to be
/// *this* service, and it has to be before `inner.start_service`, because
/// that is the call that looks addresses up in the table.
pub struct InheritedListeners<S> {
    inner: S,
    /// Addresses this process is going to listen on. Everything else in the
    /// table is by definition something the configuration no longer wants.
    wanted: Vec<String>,
}

impl<S: Service> InheritedListeners<S> {
    pub fn new(inner: S, wanted: Vec<String>) -> Self {
        Self { inner, wanted }
    }
}

#[async_trait]
impl<S: Service> Service for InheritedListeners<S> {
    async fn start_service(
        &mut self,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
    ) {
        match &fds {
            Some(table) => {
                let result = reconcile(table, &self.wanted).await;
                announce(&result, &self.wanted);
            }
            // Pingora only passes `None` when the server was never
            // bootstrapped. Nothing was inherited, so there is nothing to
            // reconcile - and nothing is silently skipped either.
            None => debug!(
                "no inherited descriptor table; every listening address is bound by this process"
            ),
        }
        self.inner
            .start_service(fds, shutdown, listeners_per_fd)
            .await
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn threads(&self) -> Option<usize> {
        self.inner.threads()
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    use pingora::server::Fds;
    use tokio::sync::Mutex as TokioMutex;

    use super::*;

    /// A listening socket and the address string the configuration would
    /// spell for it, handed over the way pingora hands one over: as a bare
    /// number, with the `TcpListener` given up.
    fn handed_over() -> (String, RawFd) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding a test listener");
        let bind = listener.local_addr().expect("the test listener's address");
        (bind.to_string(), listener.into_raw_fd())
    }

    fn table(entries: &[(String, RawFd)]) -> ListenFds {
        let mut fds = Fds::new();
        for (bind, fd) in entries {
            fds.add(bind.clone(), *fd);
        }
        Arc::new(TokioMutex::new(fds))
    }

    /// The descriptors the table names, which is what the sweep is told to
    /// leave alone. Passed explicitly in these tests so the sweep never sees
    /// a descriptor belonging to another test running beside this one.
    async fn table_fds(fds: &ListenFds) -> Vec<RawFd> {
        fds.lock().await.serialize().1
    }

    /// Make a descriptor look like one that survived an `execve`: open,
    /// listening, and without `FD_CLOEXEC`. This is exactly what pingora's
    /// `recvmsg` produces and `std::process::Command` then passes on.
    fn leaked_through_exec() -> (String, RawFd) {
        let (bind, fd) = handed_over();
        // SAFETY: clears a descriptor flag on a descriptor this test owns.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        fcntl(borrowed, FcntlArg::F_SETFD(FdFlag::empty())).expect("clearing FD_CLOEXEC");
        (bind, fd)
    }

    /// Whether the number is still open. This is the question the *next*
    /// upgrade asks: `sendmsg` with `SCM_RIGHTS` fails with `EBADF` for the
    /// whole message if one number in it is closed, and the replacement then
    /// receives nothing at all.
    fn still_open(fd: RawFd) -> bool {
        // SAFETY: only queries the descriptor flags; nothing is closed.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        fcntl(borrowed, FcntlArg::F_GETFD).is_ok()
    }

    fn is_free(bind: &str) -> bool {
        TcpListener::bind(bind).is_ok()
    }

    #[tokio::test]
    async fn a_port_dropped_from_the_configuration_is_released_rather_than_orphaned() {
        // The defect exactly: the old process hands two addresses over, the
        // new configuration names one. Without this the second stays bound,
        // listening and unattached for the life of the process, and every
        // client that reaches it hangs instead of being refused.
        let (kept, kept_fd) = handed_over();
        let (dropped, dropped_fd) = handed_over();
        let fds = table(&[(kept.clone(), kept_fd), (dropped.clone(), dropped_fd)]);
        let candidates = table_fds(&fds).await;

        let result = reconcile_with(&fds, std::slice::from_ref(&kept), Ok(candidates)).await;

        let kept_outcome = &result
            .entries
            .iter()
            .find(|(b, _)| b == &kept)
            .expect("kept")
            .1;
        assert!(
            matches!(kept_outcome, Reconciled::Kept(_)),
            "the configured address must be left for pingora to adopt: {kept_outcome:?}"
        );
        let dropped_outcome = &result
            .entries
            .iter()
            .find(|(b, _)| b == &dropped)
            .expect("dropped")
            .1;
        assert!(
            matches!(dropped_outcome, Reconciled::Released(_)),
            "the address the configuration no longer names must be released: {dropped_outcome:?}"
        );

        assert!(
            is_free(&dropped),
            "{dropped} is still held; a port removed from the configuration must stop being bound"
        );
        assert!(
            !is_free(&kept),
            "{kept} was released; the configuration still names it"
        );
    }

    #[tokio::test]
    async fn a_released_descriptor_can_still_be_carried_by_the_next_upgrade() {
        // The half that makes the fix safe. `Fds` has no way to remove a key,
        // so closing the descriptor outright would leave the map naming a
        // number that is no longer open - and the next upgrade sends every
        // number in the map through one `sendmsg`, which fails whole for one
        // bad descriptor. The replacement would receive nothing after the
        // running process had already been told it may leave.
        let (dropped, dropped_fd) = handed_over();
        let fds = table(&[(dropped.clone(), dropped_fd)]);
        let candidates = table_fds(&fds).await;

        reconcile_with(&fds, &[], Ok(candidates)).await;

        let (binds, numbers) = fds.lock().await.serialize();
        assert_eq!(binds, vec![dropped.clone()], "the key is still in the map");
        for fd in numbers {
            assert!(
                still_open(fd),
                "descriptor {fd} for {dropped} was closed; the next upgrade's SCM_RIGHTS message \
                 would fail with EBADF and hand the replacement nothing"
            );
            assert!(
                matches!(classify(fd, &dropped), Inherited::NotWhatItClaims(_)),
                "the descriptor must no longer be a listening socket at {dropped}"
            );
        }
    }

    #[tokio::test]
    async fn nothing_the_table_names_may_cross_an_exec() {
        // Where the first version of this fix stopped, and why it did not
        // work past the first reload. Pingora receives these descriptors
        // without MSG_CMSG_CLOEXEC, so without this they are inherited by the
        // next replacement through `execve` as well as being transferred to
        // it - a second copy that no reconciliation can see, holding the port
        // of an address that was dropped.
        let (kept, kept_fd) = leaked_through_exec();
        let (dropped, dropped_fd) = leaked_through_exec();
        let devnull = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .expect("opening /dev/null")
            .into_raw_fd();
        let stale = "127.0.0.1:9".to_string();
        let fds = table(&[
            (kept.clone(), kept_fd),
            (dropped.clone(), dropped_fd),
            (stale.clone(), devnull),
        ]);
        let candidates = table_fds(&fds).await;

        reconcile_with(&fds, std::slice::from_ref(&kept), Ok(candidates)).await;

        // Re-read rather than reusing the numbers from before: a rebound
        // entry has a different number than it started with, and it is the
        // number in the map now that has to be safe. Read under one lock -
        // a tokio mutex is not reentrant, so taking it twice in one
        // expression deadlocks rather than failing.
        let (binds, numbers) = fds.lock().await.serialize();
        for (bind, fd) in binds.into_iter().zip(numbers) {
            assert!(
                !crosses_exec(fd),
                "descriptor {fd} for {bind} would be inherited by the next replacement through \
                 exec; the copy it makes is what kept a dropped port bound"
            );
        }
    }

    #[tokio::test]
    async fn an_address_removed_and_later_put_back_gets_a_fresh_listener() {
        // The sibling of the fix: after one reconciliation the map holds a
        // claim about /dev/null. A configuration that re-adds the address
        // would otherwise hand /dev/null to `TcpSocket::listen`, and that
        // failure lands after the readiness byte, where it is an outage.
        let (address, fd) = handed_over();
        let fds = table(&[(address.clone(), fd)]);
        let candidates = table_fds(&fds).await;
        reconcile_with(&fds, &[], Ok(candidates.clone())).await;

        let result = reconcile_with(&fds, std::slice::from_ref(&address), Ok(candidates)).await;

        assert_eq!(
            result.entries,
            vec![(address.clone(), Reconciled::Rebound)],
            "an address that comes back must be bound afresh"
        );
        let fresh = *fds
            .lock()
            .await
            .get(&address)
            .expect("the entry is still there");
        assert_ne!(fresh, fd, "the placeholder must have been replaced");
        assert!(
            !still_open(fd),
            "the placeholder the map no longer names has to be closed, or every removal and \
             re-addition of an address leaves one more behind for the life of the fleet"
        );
        assert!(
            matches!(classify(fresh, &address), Inherited::ListeningAt(_)),
            "what the map now names has to be a real listening socket at {address}"
        );
        assert!(
            !is_free(&address),
            "{address} must be bound again by the fresh listener"
        );
    }

    #[tokio::test]
    async fn a_descriptor_that_is_not_the_listener_it_claims_is_left_alone() {
        // The guard on the whole approach. `dup3` over a number that turned
        // out to be the error log would be a far worse bug than the one being
        // fixed, so nothing is written over until the kernel has confirmed
        // what it is.
        let devnull = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .expect("opening /dev/null");
        let fd = devnull.into_raw_fd();
        let fds = table(&[("127.0.0.1:9".to_string(), fd)]);
        let candidates = table_fds(&fds).await;

        let result = reconcile_with(&fds, &[], Ok(candidates)).await;

        assert!(
            matches!(result.entries[0].1, Reconciled::LeftAlone(_)),
            "a descriptor that is not a listener must be left alone: {:?}",
            result.entries[0].1
        );
        assert!(still_open(fd), "it must not have been closed either");
    }

    #[test]
    fn an_accepted_connection_is_not_mistaken_for_the_listener_it_arrived_on() {
        // `getsockname` cannot tell these apart: a connection accepted on
        // 127.0.0.1:p has exactly 127.0.0.1:p as its local address, so an
        // address match on its own would let a live request's socket be
        // released as if it were a decommissioned listener.
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding");
        let bind = listener.local_addr().expect("address").to_string();
        let client = std::net::TcpStream::connect(&bind).expect("connecting");
        let (accepted, _) = listener.accept().expect("accepting");

        assert!(
            matches!(
                classify(listener.as_raw_fd(), &bind),
                Inherited::ListeningAt(_)
            ),
            "the listener itself is what it claims"
        );
        match classify(accepted.as_raw_fd(), &bind) {
            Inherited::NotWhatItClaims(why) => assert!(
                why.contains("not listening"),
                "the reason has to say which question separated them: {why}"
            ),
            other => panic!("an accepted connection was read as the listener: {other:?}"),
        }
        drop(client);
    }

    #[test]
    fn a_listener_that_only_crossed_an_exec_is_what_frees_a_dropped_port() {
        // Measured on the real binary before this existed: drop a port at the
        // *second* reload and it stays LISTEN, because the copy that came
        // through exec is not in pingora's table and releasing the table's
        // copy leaves it holding the port.
        let (dropped, dropped_fd) = leaked_through_exec();
        let (kept, kept_fd) = leaked_through_exec();

        let found = sweep_exec_leaks(&[dropped_fd, kept_fd], &[], std::slice::from_ref(&kept));

        assert_eq!(found.len(), 2, "both leaks have to be found: {found:?}");
        let dropped_leak = &found.iter().find(|(fd, _, _)| *fd == dropped_fd).unwrap().2;
        assert_eq!(
            dropped_leak,
            &Leaked::Closed,
            "an exec-leaked listener at an address the configuration dropped must be closed"
        );
        assert!(is_free(&dropped), "{dropped} must now be free");

        let kept_leak = &found.iter().find(|(fd, _, _)| *fd == kept_fd).unwrap().2;
        assert_eq!(
            kept_leak,
            &Leaked::Marked,
            "a duplicate of a listener the configuration still wants must be kept, not closed"
        );
        assert!(!is_free(&kept), "{kept} must still be bound");
        assert!(
            !crosses_exec(kept_fd),
            "but it must stop being inherited, or it accumulates one per reload"
        );
        drop(unsafe { OwnedFd::from_raw_fd(kept_fd) });
    }

    #[test]
    fn the_sweep_touches_nothing_this_process_opened_for_itself() {
        // The discriminator, on its own. A socket bound by this process has
        // FD_CLOEXEC, so it can never be mistaken for one that arrived
        // through exec - which is what keeps the sweep from closing a
        // listener some other part of the process is about to use.
        let (mine, mine_fd) = handed_over();
        let in_no_table: [RawFd; 0] = [];

        let found = sweep_exec_leaks(&[mine_fd], &in_no_table, &[]);

        assert!(
            found.is_empty(),
            "a descriptor with FD_CLOEXEC cannot have crossed an exec: {found:?}"
        );
        assert!(!is_free(&mine), "{mine} must still be bound");
        drop(unsafe { OwnedFd::from_raw_fd(mine_fd) });
    }

    #[test]
    fn a_descriptor_the_table_names_is_never_swept() {
        let (address, fd) = leaked_through_exec();

        let found = sweep_exec_leaks(&[fd], &[fd], &[]);

        assert!(
            found.is_empty(),
            "the table's own descriptors are reconciled, not swept: {found:?}"
        );
        assert!(!is_free(&address), "{address} must still be bound");
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }

    #[test]
    fn the_descriptors_this_process_holds_can_be_listed() {
        // `sweep_exec_leaks` is only ever as good as what it is given, and in
        // the running proxy that is /proc/self/fd.
        let (_, fd) = handed_over();
        let listed = open_descriptors().expect("/proc/self/fd is readable");
        assert!(
            listed.contains(&fd),
            "a descriptor this process holds has to be in the list"
        );
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }

    #[tokio::test]
    async fn a_start_that_inherited_nothing_changes_nothing() {
        let fds = table(&[]);
        let result = reconcile_with(&fds, &["0.0.0.0:80".to_string()], Ok(Vec::new())).await;
        assert_eq!(result, Reconciliation::default());
    }

    #[tokio::test]
    async fn a_sweep_that_cannot_run_is_reported_rather_than_skipped_silently() {
        let (address, fd) = handed_over();
        let fds = table(&[(address, fd)]);

        let result = reconcile_with(&fds, &[], Err("no /proc here".to_string())).await;

        assert_eq!(result.sweep_skipped.as_deref(), Some("no /proc here"));
    }
}
