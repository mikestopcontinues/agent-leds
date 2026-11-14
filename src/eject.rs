//! Keeps the card mounted when macOS tries to eject it.
//!
//! macOS ejects a disk that appears while the screen is locked, which the
//! card does when the Mac wakes from hibernation: the login window refuses
//! the mount and then ejects the disk, and the lights stay dark until the
//! card is taken out and put back. Disk Arbitration asks its clients to
//! approve an eject, so the daemon refuses it, and then tries to mount the
//! card until that works, which it does once the screen is unlocked.

use std::cell::RefCell;
use std::ffi::{CStr, c_void};
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFDictionary, CFRetained, CFRunLoop, CFString, CFType, Type, kCFRunLoopDefaultMode,
};
use objc2_disk_arbitration::{
    DADisk, DADissenter, DARegisterDiskEjectApprovalCallback, DASession,
    kDADiskDescriptionVolumeNameKey, kDADiskDescriptionVolumePathKey, kDADiskMountOptionDefault,
    kDAReturnNotPermitted,
};

use crate::device;

/// How often a card that is not mounted is tried again.
const RETRY: Duration = Duration::from_secs(5);
/// How long the run loop waits for Disk Arbitration before the daemon is checked on.
const CHECK_SECONDS: f64 = 0.5;
const ARBITRATION_PID: &str = "/var/run/diskarbitrationd.pid";

/// The cards whose eject was refused and that are not mounted yet.
type Unmounted = RefCell<Vec<CFRetained<DADisk>>>;

/// Refuses every eject of the card, for as long as the daemon runs.
pub fn guard() {
    // SAFETY: creating a session has no preconditions.
    let session = unsafe { DASession::new(None) };
    // SAFETY: the mode is a constant of CoreFoundation's.
    let mode = unsafe { kCFRunLoopDefaultMode };
    let (Some(session), Some(run_loop), Some(mode)) = (session, CFRunLoop::current(), mode) else {
        eprintln!("agent-leds: no session with Disk Arbitration; the card can be ejected");
        return;
    };
    let unmounted = Unmounted::default();
    // SAFETY: the callback is given `unmounted`, which lives as long as this
    // function runs, and is called only from this thread's run loop below.
    unsafe {
        DARegisterDiskEjectApprovalCallback(
            &session,
            None,
            Some(refuse),
            ptr::from_ref(&unmounted).cast_mut().cast(),
        );
        session.schedule_with_run_loop(&run_loop, mode);
    }

    let arbitration = arbitration();
    let mut retried = Instant::now();
    loop {
        CFRunLoop::run_in_mode(Some(mode), CHECK_SECONDS, false);
        // A session belongs to the Disk Arbitration daemon it was made with,
        // and is asked nothing by the one that replaces it. launchd starts
        // this daemon again, which makes a session with the new one.
        if arbitration.is_some() && self::arbitration() != arbitration {
            eprintln!(
                "agent-leds: Disk Arbitration restarted; restarting to keep hold of the card"
            );
            std::process::exit(0);
        }
        if retried.elapsed() >= RETRY {
            retried = Instant::now();
            unmounted.borrow_mut().retain(|card| {
                let wanted = !is_mounted(card);
                if wanted {
                    // SAFETY: the session the card came from is scheduled on this thread.
                    unsafe { card.mount(None, kDADiskMountOptionDefault, None, ptr::null_mut()) };
                }
                wanted
            });
        }
    }
}

/// Answers Disk Arbitration's question of whether a disk may be ejected.
unsafe extern "C-unwind" fn refuse(
    disk: NonNull<DADisk>,
    context: *mut c_void,
) -> *const DADissenter {
    // SAFETY: Disk Arbitration passes a disk that is valid for the call, and
    // the context is the one `guard` registered.
    let (disk, unmounted) = unsafe { (disk.as_ref(), &*context.cast::<Unmounted>()) };
    if !is_card(disk) {
        return ptr::null();
    }
    let name = bsd_name(disk);
    eprintln!("agent-leds: refused to eject the card ({name})");
    let mut unmounted = unmounted.borrow_mut();
    if !unmounted.iter().any(|card| bsd_name(card) == name) {
        unmounted.push(disk.retain());
    }
    let reason = CFString::from_str("agent-leds keeps the SidePulse card mounted");
    // SAFETY: creating a dissenter has no preconditions.
    let dissenter = unsafe { DADissenter::new(None, kDAReturnNotPermitted, Some(&reason)) };
    // Disk Arbitration releases the dissenter it is handed.
    CFRetained::into_raw(dissenter).as_ptr()
}

/// Whether a disk is the card: its volume is named as the card's is. The
/// card has no partitions, so the disk that is ejected is the volume itself,
/// and any other card in the reader is let go.
fn is_card(disk: &DADisk) -> bool {
    // SAFETY: the key is a constant of Disk Arbitration's.
    let name = described(disk, unsafe { kDADiskDescriptionVolumeNameKey });
    name.and_then(|name| Some(name.downcast_ref::<CFString>()?.to_string()))
        .is_some_and(|name| name.starts_with(device::NAME))
}

fn is_mounted(disk: &DADisk) -> bool {
    // SAFETY: the key is a constant of Disk Arbitration's.
    described(disk, unsafe { kDADiskDescriptionVolumePathKey }).is_some()
}

/// What Disk Arbitration knows of a disk under `key`.
fn described(disk: &DADisk, key: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: the disk is valid, and its description is keyed by strings.
    let description = unsafe { disk.description() }?;
    let description: &CFDictionary<CFString, CFType> = unsafe { description.cast_unchecked() };
    description.get(key)
}

/// The disk's name in `/dev`, such as `disk8`.
fn bsd_name(disk: &DADisk) -> String {
    // SAFETY: the name, if the disk has one, is a C string that lives as long as the disk.
    let name = unsafe { disk.bsd_name() };
    if name.is_null() {
        return "?".to_owned();
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

/// The process id of the Disk Arbitration daemon.
fn arbitration() -> Option<u32> {
    std::fs::read_to_string(ARBITRATION_PID)
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_names_its_disk_arbitration_daemon() {
        assert!(arbitration().is_some());
    }
}
